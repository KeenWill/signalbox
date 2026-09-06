//! Workspace-instruction discovery registration and turn evidence.

use crate::*;

#[derive(sqlx::FromRow)]
struct PersistedDiscoveryUsage {
    limit_set_version: i16,
    classified_entry_count: i64,
    finding_count: i64,
    candidate_source_byte_count: i64,
    elapsed_millis: i64,
    scan_complete: bool,
}

#[derive(sqlx::FromRow)]
struct PersistedCandidate {
    candidate_ordinal: i64,
    instruction_bundle_id: Uuid,
    root_kind: String,
    root_path: String,
    source_path: String,
    bundle_kind: String,
    skill_name: Option<String>,
    skill_description: Option<String>,
    source_byte_length: Decimal,
    source_hash: Vec<u8>,
}

struct PersistedCandidateExpectation<'a> {
    ordinal: i64,
    identity: signalbox_domain::InstructionBundleId,
    root_kind: &'static str,
    bundle_kind: &'static str,
    registration: &'a signalbox_domain::InstructionBundleRegistration,
}

#[derive(sqlx::FromRow)]
struct PersistedManifestHashes {
    eligibility_hash: Vec<u8>,
    admitted_set_hash: Vec<u8>,
    manifest_hash: Vec<u8>,
}

#[derive(sqlx::FromRow)]
struct PersistedDiscoveryRoot {
    root_ordinal: i64,
    root_kind: String,
    root_path: String,
}

#[derive(sqlx::FromRow)]
struct PersistedDiscoveryFinding {
    finding_ordinal: i64,
    source_path: String,
    finding_kind: String,
}

#[derive(sqlx::FromRow)]
struct PersistedEvidenceCounts {
    discovery_count: i64,
    manifest_count: i64,
    candidate_count: i64,
    bundle_count: i64,
}

#[derive(sqlx::FromRow)]
struct PersistedRollbackCounts {
    discovery_count: i64,
    root_count: i64,
    candidate_count: i64,
    bundle_count: i64,
    finding_count: i64,
    manifest_count: i64,
}

struct TruncateRejectionExpectation {
    enable_target_trigger: &'static str,
    truncate_target: &'static str,
}

#[track_caller]
fn assert_persisted_candidate(
    persisted: &PersistedCandidate,
    expected: PersistedCandidateExpectation<'_>,
) {
    assert_eq!(persisted.candidate_ordinal, expected.ordinal);
    assert_eq!(
        persisted.instruction_bundle_id,
        expected.identity.into_uuid()
    );
    assert_eq!(persisted.root_kind, expected.root_kind);
    assert_eq!(
        persisted.root_path,
        expected.registration.root_path().as_str()
    );
    assert_eq!(
        persisted.source_path,
        expected.registration.source_path().absolute_path()
    );
    assert_eq!(persisted.bundle_kind, expected.bundle_kind);
    assert_eq!(
        persisted.skill_name.as_deref(),
        expected
            .registration
            .skill()
            .map(signalbox_domain::InstructionSkillMetadata::name)
    );
    assert_eq!(
        persisted.skill_description.as_deref(),
        expected
            .registration
            .skill()
            .map(signalbox_domain::InstructionSkillMetadata::description)
    );
    assert_eq!(
        persisted.source_byte_length,
        Decimal::from(expected.registration.source_bytes())
    );
    assert_eq!(
        persisted.source_hash,
        expected.registration.source_hash().as_bytes().as_slice()
    );
}

#[track_caller]
fn assert_append_only_rejection(result: Result<sqlx::postgres::PgQueryResult, sqlx::Error>) {
    let error = result.expect_err("append-only evidence rejects mutation");
    let database = error
        .as_database_error()
        .expect("append-only rejection is a database error");
    assert_eq!(database.code().as_deref(), Some("23514"));
}

#[track_caller]
fn assert_truncate_rejection<'a>(
    pool: &'a PgPool,
    expectation: TruncateRejectionExpectation,
) -> impl std::future::Future<Output = Result<(), Box<dyn Error>>> + 'a {
    let caller = std::panic::Location::caller();
    async move {
        let mut transaction = pool.begin().await?;
        sqlx::query("SET LOCAL session_replication_role = 'replica'")
            .execute(&mut *transaction)
            .await?;
        sqlx::query(expectation.enable_target_trigger)
            .execute(&mut *transaction)
            .await?;
        let error = match sqlx::query(expectation.truncate_target)
            .execute(&mut *transaction)
            .await
        {
            Ok(_) => panic!("append-only evidence accepted truncation requested at {caller}"),
            Err(error) => error,
        };
        transaction.rollback().await?;
        let database = error.as_database_error().unwrap_or_else(|| {
            panic!("truncate rejection requested at {caller} was not a database error")
        });
        assert_eq!(
            database.code().as_deref(),
            Some("23514"),
            "truncate rejection requested at {caller} used the wrong SQLSTATE"
        );
        Ok(())
    }
}

async fn queued_instruction_turn(
    pool: &PgPool,
    identity_base: u128,
) -> Result<(SessionId, TurnId), Box<dyn Error>> {
    let session = SessionId::from_uuid(Uuid::from_u128(identity_base + 1));
    let selection = DirectModelSelection::from_uuid(Uuid::from_u128(identity_base + 2));
    let mut create = CreateSessionService::new(
        FixedSessionIds::new([session]),
        CreateSessionRepository::new(pool.clone(), test_session_credential_pin()),
    );
    let CreateSessionOutcome::Applied(_) = create
        .execute(CreateSessionRequest::try_new(
            DurableCommandId::from_uuid(Uuid::from_u128(identity_base + 3)),
            SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(selection)),
        )?)
        .await?
    else {
        panic!("the retry fixture session must be created");
    };
    let turn = TurnId::from_uuid(Uuid::from_u128(identity_base + 4));
    let mut submit = SubmitInputService::new(
        FixedSubmitInputIds::new(
            [AcceptedInputId::from_uuid(Uuid::from_u128(
                identity_base + 5,
            ))],
            [turn],
        ),
        SubmitInputRepository::new(pool.clone()),
        AcceptingEligibilityNudge,
        signalbox_application::InProcessToolDispatchGate::default(),
    );
    let SubmitInputOutcome::Recorded(SubmitInputResult::Applied(
        SubmitInputAppliedResult::TurnOrigin(_),
    )) = submit
        .execute(SubmitInputRequest::try_new(
            DurableCommandId::from_uuid(Uuid::from_u128(identity_base + 6)),
            session,
            UserContent::try_text("retry workspace discovery".to_owned())
                .expect("fixture user content is admitted"),
            DeliveryRequest::StartWhenNoActiveTurn {
                configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
            },
        )?)
        .await?
    else {
        panic!("the retry fixture input must be accepted");
    };
    Ok((session, turn))
}

async fn active_instruction_turn(
    pool: &PgPool,
    identity_base: u128,
) -> Result<(SessionId, TurnId), Box<dyn Error>> {
    let (session, turn) = queued_instruction_turn(pool, identity_base).await?;
    let mut activation = StartEligibleTurnService::new(
        FixedStartEligibleTurnIds::new(
            [SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                identity_base + 7,
            ))],
            [ContextFrontierId::from_uuid(Uuid::from_u128(
                identity_base + 8,
            ))],
            [TurnAttemptId::from_uuid(Uuid::from_u128(identity_base + 9))],
        ),
        StartEligibleTurnRepository::new(pool.clone()),
    );
    let StartEligibleTurnOutcome::Activated(activated) = activation.execute(session).await? else {
        panic!("the retry fixture turn must activate");
    };
    assert_eq!(activated.turn(), turn);
    Ok((session, turn))
}

/// one active turn records its exact discovery evidence and empty
/// turn-start instruction manifest before model execution.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn turn_instruction_snapshot_is_exact() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = SessionId::from_uuid(Uuid::from_u128(0x6101));
    let selection = DirectModelSelection::from_uuid(Uuid::from_u128(0x6102));
    let mut create = CreateSessionService::new(
        FixedSessionIds::new([session]),
        CreateSessionRepository::new(pool.clone(), test_session_credential_pin()),
    );
    let CreateSessionOutcome::Applied(_) = create
        .execute(CreateSessionRequest::try_new(
            DurableCommandId::from_uuid(Uuid::from_u128(0x6103)),
            SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(selection)),
        )?)
        .await?
    else {
        panic!("the instruction fixture session must be created");
    };
    let turn = TurnId::from_uuid(Uuid::from_u128(0x6104));
    let mut submit = SubmitInputService::new(
        FixedSubmitInputIds::new(
            [AcceptedInputId::from_uuid(Uuid::from_u128(0x6105))],
            [turn],
        ),
        SubmitInputRepository::new(pool.clone()),
        AcceptingEligibilityNudge,
        signalbox_application::InProcessToolDispatchGate::default(),
    );
    let SubmitInputOutcome::Recorded(SubmitInputResult::Applied(
        SubmitInputAppliedResult::TurnOrigin(_),
    )) = submit
        .execute(SubmitInputRequest::try_new(
            DurableCommandId::from_uuid(Uuid::from_u128(0x6106)),
            session,
            UserContent::try_text("inspect workspace instructions".to_owned())
                .expect("fixture user content is admitted"),
            DeliveryRequest::StartWhenNoActiveTurn {
                configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
            },
        )?)
        .await?
    else {
        panic!("the instruction fixture input must be accepted");
    };
    let mut activation = StartEligibleTurnService::new(
        FixedStartEligibleTurnIds::new(
            [SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                0x6107,
            ))],
            [ContextFrontierId::from_uuid(Uuid::from_u128(0x6108))],
            [TurnAttemptId::from_uuid(Uuid::from_u128(0x6109))],
        ),
        StartEligibleTurnRepository::new(pool.clone()),
    );
    let StartEligibleTurnOutcome::Activated(activated) = activation.execute(session).await? else {
        panic!("the instruction fixture turn must activate");
    };
    assert_eq!(activated.turn(), turn);

    let directory = tempfile::tempdir()?;
    let root = directory.path().canonicalize()?;
    std::fs::write(root.join("AGENTS.md"), "workspace rule\n")?;
    std::fs::create_dir_all(root.join(".agents/skills/review"))?;
    std::fs::write(
        root.join(".agents/skills/review/SKILL.md"),
        "---\nname: review\ndescription: Review one change\n---\nsteps\n",
    )?;
    std::fs::create_dir_all(root.join(".agents/skills/broken"))?;
    std::fs::write(
        root.join(".agents/skills/broken/SKILL.md"),
        "missing frontmatter\n",
    )?;
    let configured_directory = tempfile::tempdir()?;
    let configured_root_path = configured_directory.path().canonicalize()?;
    std::fs::write(configured_root_path.join("AGENTS.md"), "configured rule\n")?;
    let root = signalbox_domain::InstructionPath::try_new(
        root.to_str().expect("temporary path is UTF-8").to_owned(),
    )?;
    let configured_root = signalbox_domain::InstructionPath::try_new(
        configured_root_path
            .to_str()
            .expect("configured temporary path is UTF-8")
            .to_owned(),
    )?;
    let snapshot = signalbox_application::discover_workspace_instructions(vec![
        signalbox_application::InstructionDiscoveryRoot::new(
            signalbox_domain::InstructionDiscoveryRootKind::Workspace,
            root,
        ),
        signalbox_application::InstructionDiscoveryRoot::new(
            signalbox_domain::InstructionDiscoveryRootKind::Configured,
            configured_root,
        ),
    ]);
    let discovery = signalbox_domain::InstructionDiscoveryId::from_uuid(Uuid::from_u128(0x6110));
    let manifest_id =
        signalbox_domain::TurnInstructionManifestId::from_uuid(Uuid::from_u128(0x6111));
    let manifest =
        signalbox_domain::TurnInstructionManifest::empty_turn_start(manifest_id, session, turn);
    let agent_document_id =
        signalbox_domain::InstructionBundleId::from_uuid(Uuid::from_u128(0x6112));
    let agent_skill_id = signalbox_domain::InstructionBundleId::from_uuid(Uuid::from_u128(0x6113));
    let configured_document_id =
        signalbox_domain::InstructionBundleId::from_uuid(Uuid::from_u128(0x6114));
    let mut bundle_ids = [agent_document_id, agent_skill_id, configured_document_id].into_iter();
    let outcome =
        signalbox_persistence::workspace_instructions::WorkspaceInstructionRepository::new(
            pool.clone(),
        )
        .record_turn_start(discovery, manifest.clone(), &snapshot, || {
            bundle_ids
                .next()
                .expect("three discovered bundles need three identities")
        })
        .await?;
    assert_eq!(
        outcome,
        signalbox_persistence::workspace_instructions::RecordTurnInstructionSnapshotOutcome::Recorded(
            manifest_id,
        )
    );
    let persisted_usage = sqlx::query_as::<_, PersistedDiscoveryUsage>(
        "SELECT limit_set_version, classified_entry_count, finding_count,
                candidate_source_byte_count, elapsed_millis, scan_complete
           FROM instruction_discovery
          WHERE instruction_discovery_id = $1",
    )
    .bind(discovery.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        persisted_usage.limit_set_version,
        i16::try_from(snapshot.limit_set_version()).expect("fixture limit version fits smallint")
    );
    assert_eq!(
        persisted_usage.classified_entry_count,
        i64::try_from(snapshot.classified_entries()).expect("fixture entry count fits bigint")
    );
    assert_eq!(
        persisted_usage.finding_count,
        i64::try_from(snapshot.findings().len()).expect("fixture finding count fits bigint")
    );
    assert_eq!(
        persisted_usage.candidate_source_byte_count,
        i64::try_from(snapshot.candidate_source_bytes())
            .expect("fixture source byte count fits bigint")
    );
    assert_eq!(
        persisted_usage.elapsed_millis,
        i64::try_from(snapshot.elapsed_millis()).expect("fixture elapsed time fits bigint")
    );
    assert_eq!(persisted_usage.scan_complete, snapshot.is_complete());
    let persisted_roots = sqlx::query_as::<_, PersistedDiscoveryRoot>(
        "SELECT root_ordinal, root_kind, root_path
           FROM instruction_discovery_root
          WHERE instruction_discovery_id = $1
          ORDER BY root_ordinal",
    )
    .bind(discovery.into_uuid())
    .fetch_all(&pool)
    .await?;
    let persisted_workspace_root = persisted_roots
        .first()
        .expect("the workspace discovery root is persisted");
    let expected_workspace_root = snapshot
        .roots()
        .first()
        .expect("the discovery contains the workspace root");
    let persisted_configured_root = persisted_roots
        .get(1)
        .expect("the configured discovery root is persisted");
    let expected_configured_root = snapshot
        .roots()
        .get(1)
        .expect("the discovery contains the configured root");
    assert_eq!(persisted_roots.len(), snapshot.roots().len());
    assert_eq!(persisted_workspace_root.root_ordinal, 1);
    assert_eq!(persisted_workspace_root.root_kind, "workspace");
    assert_eq!(
        persisted_workspace_root.root_path,
        expected_workspace_root.path().as_str()
    );
    assert_eq!(persisted_configured_root.root_ordinal, 2);
    assert_eq!(persisted_configured_root.root_kind, "configured");
    assert_eq!(
        persisted_configured_root.root_path,
        expected_configured_root.path().as_str()
    );
    let persisted_candidates = sqlx::query_as::<_, PersistedCandidate>(
        "SELECT candidate.candidate_ordinal, bundle.instruction_bundle_id,
                bundle.root_kind, bundle.root_path, bundle.source_path,
                bundle.bundle_kind, bundle.skill_name, bundle.skill_description,
                bundle.source_byte_length, bundle.source_hash
           FROM instruction_discovery_candidate AS candidate
           JOIN registered_instruction_bundle AS bundle
             ON bundle.instruction_bundle_id = candidate.instruction_bundle_id
          WHERE candidate.instruction_discovery_id = $1
          ORDER BY candidate.candidate_ordinal",
    )
    .bind(discovery.into_uuid())
    .fetch_all(&pool)
    .await?;
    assert_eq!(persisted_candidates.len(), snapshot.bundles().len());
    assert_persisted_candidate(
        persisted_candidates
            .first()
            .expect("the agent document candidate is persisted"),
        PersistedCandidateExpectation {
            ordinal: 1,
            identity: agent_document_id,
            root_kind: "workspace",
            bundle_kind: "agent_document",
            registration: snapshot
                .bundles()
                .first()
                .expect("the discovery contains the agent document"),
        },
    );
    assert_persisted_candidate(
        persisted_candidates
            .get(1)
            .expect("the skill candidate is persisted"),
        PersistedCandidateExpectation {
            ordinal: 2,
            identity: agent_skill_id,
            root_kind: "workspace",
            bundle_kind: "agent_skill",
            registration: snapshot
                .bundles()
                .get(1)
                .expect("the discovery contains the skill"),
        },
    );
    assert_persisted_candidate(
        persisted_candidates
            .get(2)
            .expect("the configured agent document candidate is persisted"),
        PersistedCandidateExpectation {
            ordinal: 3,
            identity: configured_document_id,
            root_kind: "configured",
            bundle_kind: "agent_document",
            registration: snapshot
                .bundles()
                .get(2)
                .expect("the discovery contains the configured agent document"),
        },
    );
    let persisted_findings = sqlx::query_as::<_, PersistedDiscoveryFinding>(
        "SELECT finding_ordinal, source_path, finding_kind
           FROM instruction_discovery_finding
          WHERE instruction_discovery_id = $1
          ORDER BY finding_ordinal",
    )
    .bind(discovery.into_uuid())
    .fetch_all(&pool)
    .await?;
    let persisted_finding = persisted_findings
        .first()
        .expect("the invalid skill finding is persisted");
    let expected_finding = snapshot
        .findings()
        .first()
        .expect("the discovery carries the invalid skill finding");
    assert_eq!(persisted_findings.len(), snapshot.findings().len());
    assert_eq!(persisted_finding.finding_ordinal, 1);
    assert_eq!(
        persisted_finding.source_path,
        expected_finding.path().as_str()
    );
    assert_eq!(
        expected_finding.kind(),
        signalbox_application::InstructionDiscoveryFindingKind::InvalidSkill
    );
    assert_eq!(persisted_finding.finding_kind, "invalid_skill");
    let persisted_hashes = sqlx::query_as::<_, PersistedManifestHashes>(
        "SELECT eligibility_hash, admitted_set_hash, manifest_hash
           FROM turn_instruction_manifest
          WHERE turn_instruction_manifest_id = $1",
    )
    .bind(manifest_id.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        persisted_hashes.eligibility_hash,
        manifest.eligibility_hash().as_bytes().as_slice()
    );
    assert_eq!(
        persisted_hashes.admitted_set_hash,
        manifest.admitted_set_hash().as_bytes().as_slice()
    );
    assert_eq!(
        persisted_hashes.manifest_hash,
        manifest.manifest_hash().as_bytes().as_slice()
    );
    assert_eq!(
        signalbox_persistence::workspace_instructions::WorkspaceInstructionRepository::new(
            pool.clone(),
        )
        .preflight_turn_start(session, turn)
        .await?,
        signalbox_persistence::workspace_instructions::TurnInstructionManifestPreflight::Available(
            manifest_id,
        )
    );
    pool.close().await;
    drop(container);
    Ok(())
}

/// a complete recorder that loses scheduler serialization observes
/// the winning manifest and retains none of its fresh evidence identities.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn losing_complete_record_observes_the_winning_manifest() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let arbitrary_replay_identity_base = 0x6900;
    let (session, turn) = active_instruction_turn(&pool, arbitrary_replay_identity_base).await?;
    let directory = tempfile::tempdir()?;
    let root_path = directory.path().canonicalize()?;
    std::fs::write(root_path.join("AGENTS.md"), "winning rule\n")?;
    let root = signalbox_domain::InstructionPath::try_new(
        root_path
            .to_str()
            .expect("temporary path is UTF-8")
            .to_owned(),
    )?;
    let snapshot = signalbox_application::discover_workspace_instructions(vec![
        signalbox_application::InstructionDiscoveryRoot::new(
            signalbox_domain::InstructionDiscoveryRootKind::Workspace,
            root,
        ),
    ]);
    let repository =
        signalbox_persistence::workspace_instructions::WorkspaceInstructionRepository::new(
            pool.clone(),
        );
    let winning_discovery =
        signalbox_domain::InstructionDiscoveryId::from_uuid(Uuid::from_u128(0x6910));
    let winning_manifest_id =
        signalbox_domain::TurnInstructionManifestId::from_uuid(Uuid::from_u128(0x6911));
    let winning_bundle = signalbox_domain::InstructionBundleId::from_uuid(Uuid::from_u128(0x6912));
    let winning = repository
        .record_turn_start(
            winning_discovery,
            signalbox_domain::TurnInstructionManifest::empty_turn_start(
                winning_manifest_id,
                session,
                turn,
            ),
            &snapshot,
            || winning_bundle,
        )
        .await?;
    let losing_discovery =
        signalbox_domain::InstructionDiscoveryId::from_uuid(Uuid::from_u128(0x6920));
    let losing_manifest_id =
        signalbox_domain::TurnInstructionManifestId::from_uuid(Uuid::from_u128(0x6921));
    let losing_bundle = signalbox_domain::InstructionBundleId::from_uuid(Uuid::from_u128(0x6922));
    let losing = repository
        .record_turn_start(
            losing_discovery,
            signalbox_domain::TurnInstructionManifest::empty_turn_start(
                losing_manifest_id,
                session,
                turn,
            ),
            &snapshot,
            || losing_bundle,
        )
        .await?;
    let evidence_counts = sqlx::query_as::<_, PersistedEvidenceCounts>(
        "SELECT
            (SELECT count(*) FROM instruction_discovery
              WHERE session_id = $1 AND turn_id = $2) AS discovery_count,
            (SELECT count(*) FROM turn_instruction_manifest
              WHERE session_id = $1 AND turn_id = $2) AS manifest_count,
            (SELECT count(*) FROM instruction_discovery_candidate AS candidate
              JOIN instruction_discovery AS discovery
                ON discovery.instruction_discovery_id = candidate.instruction_discovery_id
             WHERE discovery.session_id = $1 AND discovery.turn_id = $2) AS candidate_count,
            (SELECT count(*) FROM registered_instruction_bundle
              WHERE root_path = $3) AS bundle_count",
    )
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .bind(snapshot.roots()[0].path().as_str())
    .fetch_one(&pool)
    .await?;

    assert_eq!(
        winning,
        signalbox_persistence::workspace_instructions::RecordTurnInstructionSnapshotOutcome::Recorded(
            winning_manifest_id,
        )
    );
    assert_eq!(
        losing,
        signalbox_persistence::workspace_instructions::RecordTurnInstructionSnapshotOutcome::AlreadyRecorded(
            winning_manifest_id,
        )
    );
    assert_eq!(evidence_counts.discovery_count, 1);
    assert_eq!(evidence_counts.manifest_count, 1);
    assert_eq!(evidence_counts.candidate_count, 1);
    assert_eq!(evidence_counts.bundle_count, 1);

    pool.close().await;
    drop(container);
    Ok(())
}

/// a failure after the recorder has inserted discovery, root, bundle,
/// and candidate evidence rolls the entire attempted snapshot back.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn mid_record_failure_rolls_back_all_instruction_evidence() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let (session, turn) = active_instruction_turn(&pool, 0x6a00).await?;
    let directory = tempfile::tempdir()?;
    let root_path = directory.path().canonicalize()?;
    std::fs::write(root_path.join("AGENTS.md"), "workspace rule\n")?;
    std::fs::create_dir_all(root_path.join(".agents/skills/review"))?;
    std::fs::write(
        root_path.join(".agents/skills/review/SKILL.md"),
        "---\nname: review\ndescription: Review one change\n---\nsteps\n",
    )?;
    std::fs::create_dir_all(root_path.join(".agents/skills/broken"))?;
    std::fs::write(
        root_path.join(".agents/skills/broken/SKILL.md"),
        "missing frontmatter\n",
    )?;
    let root = signalbox_domain::InstructionPath::try_new(
        root_path
            .to_str()
            .expect("temporary path is UTF-8")
            .to_owned(),
    )?;
    let snapshot = signalbox_application::discover_workspace_instructions(vec![
        signalbox_application::InstructionDiscoveryRoot::new(
            signalbox_domain::InstructionDiscoveryRootKind::Workspace,
            root,
        ),
    ]);
    let discovery = signalbox_domain::InstructionDiscoveryId::from_uuid(Uuid::from_u128(0x6a10));
    let manifest_id =
        signalbox_domain::TurnInstructionManifestId::from_uuid(Uuid::from_u128(0x6a11));
    let duplicate_bundle =
        signalbox_domain::InstructionBundleId::from_uuid(Uuid::from_u128(0x6a12));
    let result =
        signalbox_persistence::workspace_instructions::WorkspaceInstructionRepository::new(
            pool.clone(),
        )
        .record_turn_start(
            discovery,
            signalbox_domain::TurnInstructionManifest::empty_turn_start(manifest_id, session, turn),
            &snapshot,
            || duplicate_bundle,
        )
        .await;
    let counts = sqlx::query_as::<_, PersistedRollbackCounts>(
        "SELECT
            (SELECT count(*) FROM instruction_discovery
              WHERE instruction_discovery_id = $1) AS discovery_count,
            (SELECT count(*) FROM instruction_discovery_root
              WHERE instruction_discovery_id = $1) AS root_count,
            (SELECT count(*) FROM instruction_discovery_candidate
              WHERE instruction_discovery_id = $1) AS candidate_count,
            (SELECT count(*) FROM registered_instruction_bundle
              WHERE root_path = $2) AS bundle_count,
            (SELECT count(*) FROM instruction_discovery_finding
              WHERE instruction_discovery_id = $1) AS finding_count,
            (SELECT count(*) FROM turn_instruction_manifest
              WHERE turn_instruction_manifest_id = $3) AS manifest_count",
    )
    .bind(discovery.into_uuid())
    .bind(snapshot.roots()[0].path().as_str())
    .bind(manifest_id.into_uuid())
    .fetch_one(&pool)
    .await?;

    assert_eq!(snapshot.bundles().len(), 2);
    assert_eq!(snapshot.findings().len(), 1);
    let error = result.expect_err("the duplicate bundle identity aborts the snapshot transaction");
    let sqlx_error = error
        .source()
        .expect("the repository error retains its database source")
        .downcast_ref::<sqlx::Error>()
        .expect("the repository database source remains a sqlx error");
    let database_error = sqlx_error
        .as_database_error()
        .expect("the duplicate identity is a database uniqueness violation");
    assert_eq!(database_error.code().as_deref(), Some("23505"));
    assert_eq!(
        database_error.constraint(),
        Some("registered_instruction_bundle_pkey")
    );
    assert_eq!(counts.discovery_count, 0);
    assert_eq!(counts.root_count, 0);
    assert_eq!(counts.candidate_count, 0);
    assert_eq!(counts.bundle_count, 0);
    assert_eq!(counts.finding_count, 0);
    assert_eq!(counts.manifest_count, 0);

    pool.close().await;
    drop(container);
    Ok(())
}

/// every persisted turn-instruction evidence table rejects mutation.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn turn_instruction_evidence_is_append_only() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let (session, turn) = active_instruction_turn(&pool, 0x6700).await?;
    let directory = tempfile::tempdir()?;
    let root_path = directory.path().canonicalize()?;
    std::fs::write(root_path.join("AGENTS.md"), "workspace rule\n")?;
    std::fs::create_dir_all(root_path.join(".agents/skills/review"))?;
    std::fs::write(
        root_path.join(".agents/skills/review/SKILL.md"),
        "---\nname: review\ndescription: Review one change\n---\nsteps\n",
    )?;
    std::fs::create_dir_all(root_path.join(".agents/skills/broken"))?;
    std::fs::write(
        root_path.join(".agents/skills/broken/SKILL.md"),
        "missing frontmatter\n",
    )?;
    let root = signalbox_domain::InstructionPath::try_new(
        root_path
            .to_str()
            .expect("temporary path is UTF-8")
            .to_owned(),
    )?;
    let snapshot = signalbox_application::discover_workspace_instructions(vec![
        signalbox_application::InstructionDiscoveryRoot::new(
            signalbox_domain::InstructionDiscoveryRootKind::Workspace,
            root,
        ),
    ]);
    let discovery = signalbox_domain::InstructionDiscoveryId::from_uuid(Uuid::from_u128(0x6710));
    let manifest_id =
        signalbox_domain::TurnInstructionManifestId::from_uuid(Uuid::from_u128(0x6711));
    let manifest =
        signalbox_domain::TurnInstructionManifest::empty_turn_start(manifest_id, session, turn);
    let agent_document_id =
        signalbox_domain::InstructionBundleId::from_uuid(Uuid::from_u128(0x6712));
    let agent_skill_id = signalbox_domain::InstructionBundleId::from_uuid(Uuid::from_u128(0x6713));
    let mut bundle_ids = [agent_document_id, agent_skill_id].into_iter();
    let outcome =
        signalbox_persistence::workspace_instructions::WorkspaceInstructionRepository::new(
            pool.clone(),
        )
        .record_turn_start(discovery, manifest, &snapshot, || {
            bundle_ids
                .next()
                .expect("two discovered bundles need two identities")
        })
        .await?;
    assert_eq!(
        outcome,
        signalbox_persistence::workspace_instructions::RecordTurnInstructionSnapshotOutcome::Recorded(
            manifest_id,
        )
    );

    assert_truncate_rejection(
        &pool,
        TruncateRejectionExpectation {
            enable_target_trigger: "ALTER TABLE instruction_discovery ENABLE ALWAYS TRIGGER instruction_discovery_rejects_truncate",
            truncate_target: "TRUNCATE TABLE instruction_discovery CASCADE",
        },
    )
    .await?;
    assert_truncate_rejection(
        &pool,
        TruncateRejectionExpectation {
            enable_target_trigger: "ALTER TABLE instruction_discovery_root ENABLE ALWAYS TRIGGER instruction_discovery_root_rejects_truncate",
            truncate_target: "TRUNCATE TABLE instruction_discovery_root CASCADE",
        },
    )
    .await?;
    assert_truncate_rejection(
        &pool,
        TruncateRejectionExpectation {
            enable_target_trigger: "ALTER TABLE registered_instruction_bundle ENABLE ALWAYS TRIGGER registered_instruction_bundle_rejects_truncate",
            truncate_target: "TRUNCATE TABLE registered_instruction_bundle CASCADE",
        },
    )
    .await?;
    assert_truncate_rejection(
        &pool,
        TruncateRejectionExpectation {
            enable_target_trigger: "ALTER TABLE instruction_discovery_candidate ENABLE ALWAYS TRIGGER instruction_discovery_candidate_rejects_truncate",
            truncate_target: "TRUNCATE TABLE instruction_discovery_candidate CASCADE",
        },
    )
    .await?;
    assert_truncate_rejection(
        &pool,
        TruncateRejectionExpectation {
            enable_target_trigger: "ALTER TABLE instruction_discovery_finding ENABLE ALWAYS TRIGGER instruction_discovery_finding_rejects_truncate",
            truncate_target: "TRUNCATE TABLE instruction_discovery_finding CASCADE",
        },
    )
    .await?;
    assert_truncate_rejection(
        &pool,
        TruncateRejectionExpectation {
            enable_target_trigger: "ALTER TABLE turn_instruction_manifest ENABLE ALWAYS TRIGGER turn_instruction_manifest_rejects_truncate",
            truncate_target: "TRUNCATE TABLE turn_instruction_manifest CASCADE",
        },
    )
    .await?;

    let discovery_update = sqlx::query(
        "UPDATE instruction_discovery SET elapsed_millis = elapsed_millis
          WHERE instruction_discovery_id = $1",
    )
    .bind(discovery.into_uuid())
    .execute(&pool)
    .await;
    assert_append_only_rejection(discovery_update);
    let discovery_delete =
        sqlx::query("DELETE FROM instruction_discovery WHERE instruction_discovery_id = $1")
            .bind(discovery.into_uuid())
            .execute(&pool)
            .await;
    assert_append_only_rejection(discovery_delete);
    let root_update = sqlx::query(
        "UPDATE instruction_discovery_root SET root_kind = root_kind
          WHERE instruction_discovery_id = $1 AND root_ordinal = 1",
    )
    .bind(discovery.into_uuid())
    .execute(&pool)
    .await;
    assert_append_only_rejection(root_update);
    let root_delete = sqlx::query(
        "DELETE FROM instruction_discovery_root
          WHERE instruction_discovery_id = $1 AND root_ordinal = 1",
    )
    .bind(discovery.into_uuid())
    .execute(&pool)
    .await;
    assert_append_only_rejection(root_delete);
    let candidate_update = sqlx::query(
        "UPDATE instruction_discovery_candidate
            SET candidate_ordinal = candidate_ordinal
          WHERE instruction_discovery_id = $1 AND candidate_ordinal = 1",
    )
    .bind(discovery.into_uuid())
    .execute(&pool)
    .await;
    assert_append_only_rejection(candidate_update);
    let candidate_delete = sqlx::query(
        "DELETE FROM instruction_discovery_candidate
          WHERE instruction_discovery_id = $1 AND candidate_ordinal = 1",
    )
    .bind(discovery.into_uuid())
    .execute(&pool)
    .await;
    assert_append_only_rejection(candidate_delete);
    let bundle_update = sqlx::query(
        "UPDATE registered_instruction_bundle SET source_hash = source_hash
          WHERE instruction_bundle_id = $1",
    )
    .bind(agent_document_id.into_uuid())
    .execute(&pool)
    .await;
    assert_append_only_rejection(bundle_update);
    let bundle_delete =
        sqlx::query("DELETE FROM registered_instruction_bundle WHERE instruction_bundle_id = $1")
            .bind(agent_document_id.into_uuid())
            .execute(&pool)
            .await;
    assert_append_only_rejection(bundle_delete);
    let finding_update = sqlx::query(
        "UPDATE instruction_discovery_finding SET finding_kind = finding_kind
          WHERE instruction_discovery_id = $1 AND finding_ordinal = 1",
    )
    .bind(discovery.into_uuid())
    .execute(&pool)
    .await;
    assert_append_only_rejection(finding_update);
    let finding_delete = sqlx::query(
        "DELETE FROM instruction_discovery_finding
          WHERE instruction_discovery_id = $1 AND finding_ordinal = 1",
    )
    .bind(discovery.into_uuid())
    .execute(&pool)
    .await;
    assert_append_only_rejection(finding_delete);
    let manifest_update = sqlx::query(
        "UPDATE turn_instruction_manifest SET boundary_kind = 'turn_start'
          WHERE turn_instruction_manifest_id = $1",
    )
    .bind(manifest_id.into_uuid())
    .execute(&pool)
    .await;
    assert_append_only_rejection(manifest_update);
    let manifest_delete = sqlx::query(
        "DELETE FROM turn_instruction_manifest WHERE turn_instruction_manifest_id = $1",
    )
    .bind(manifest_id.into_uuid())
    .execute(&pool)
    .await;
    assert_append_only_rejection(manifest_delete);

    pool.close().await;
    drop(container);
    Ok(())
}

/// counted activation records a nonempty discovery while the selected
/// turn is still queued, and the active-turn boundary does not accept it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn counted_activation_records_a_queued_turn_manifest() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let (session, turn) = queued_instruction_turn(&pool, 0x6300).await?;
    let directory = tempfile::tempdir()?;
    let root_path = directory.path().canonicalize()?;
    std::fs::write(root_path.join("AGENTS.md"), "counted activation rule\n")?;
    let root = signalbox_domain::InstructionPath::try_new(
        root_path
            .to_str()
            .expect("temporary path is UTF-8")
            .to_owned(),
    )?;
    let snapshot = signalbox_application::discover_workspace_instructions(vec![
        signalbox_application::InstructionDiscoveryRoot::new(
            signalbox_domain::InstructionDiscoveryRootKind::Workspace,
            root,
        ),
    ]);
    let repository =
        signalbox_persistence::workspace_instructions::WorkspaceInstructionRepository::new(
            pool.clone(),
        );
    let discovery = signalbox_domain::InstructionDiscoveryId::from_uuid(Uuid::from_u128(0x6310));
    let manifest_id =
        signalbox_domain::TurnInstructionManifestId::from_uuid(Uuid::from_u128(0x6311));
    let manifest =
        signalbox_domain::TurnInstructionManifest::empty_turn_start(manifest_id, session, turn);
    let bundle_id = signalbox_domain::InstructionBundleId::from_uuid(Uuid::from_u128(0x6312));

    assert!(snapshot.is_complete());
    assert_eq!(
        repository
            .preflight_counted_activation(session, turn)
            .await?,
        signalbox_persistence::workspace_instructions::TurnInstructionManifestPreflight::Absent
    );
    assert_eq!(
        repository
            .record_turn_start(discovery, manifest.clone(), &snapshot, || bundle_id)
            .await?,
        signalbox_persistence::workspace_instructions::RecordTurnInstructionSnapshotOutcome::TurnUnavailable
    );
    assert_eq!(
        repository
            .record_counted_activation(discovery, manifest, &snapshot, || bundle_id)
            .await?,
        signalbox_persistence::workspace_instructions::RecordTurnInstructionSnapshotOutcome::Recorded(
            manifest_id,
        )
    );
    assert_eq!(
        repository
            .preflight_counted_activation(session, turn)
            .await?,
        signalbox_persistence::workspace_instructions::TurnInstructionManifestPreflight::Available(
            manifest_id,
        )
    );
    let persisted_candidate = sqlx::query_as::<_, PersistedCandidate>(
        "SELECT candidate.candidate_ordinal, bundle.instruction_bundle_id,
                bundle.root_kind, bundle.root_path, bundle.source_path,
                bundle.bundle_kind, bundle.skill_name, bundle.skill_description,
                bundle.source_byte_length, bundle.source_hash
           FROM instruction_discovery_candidate AS candidate
           JOIN registered_instruction_bundle AS bundle
             ON bundle.instruction_bundle_id = candidate.instruction_bundle_id
          WHERE candidate.instruction_discovery_id = $1
            AND candidate.candidate_ordinal = 1",
    )
    .bind(discovery.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_persisted_candidate(
        &persisted_candidate,
        PersistedCandidateExpectation {
            ordinal: 1,
            identity: bundle_id,
            root_kind: "workspace",
            bundle_kind: "agent_document",
            registration: snapshot
                .bundles()
                .first()
                .expect("the counted discovery contains the agent document"),
        },
    );
    assert_eq!(
        repository.preflight_turn_start(session, turn).await?,
        signalbox_persistence::workspace_instructions::TurnInstructionManifestPreflight::TurnUnavailable
    );
    let (active_session, active_turn) = active_instruction_turn(&pool, 0x6330).await?;
    let active_discovery =
        signalbox_domain::InstructionDiscoveryId::from_uuid(Uuid::from_u128(0x6340));
    let active_manifest_id =
        signalbox_domain::TurnInstructionManifestId::from_uuid(Uuid::from_u128(0x6341));
    let active_manifest = signalbox_domain::TurnInstructionManifest::empty_turn_start(
        active_manifest_id,
        active_session,
        active_turn,
    );
    assert_eq!(
        repository
            .record_counted_activation(
                active_discovery,
                active_manifest,
                &snapshot,
                || bundle_id,
            )
            .await?,
        signalbox_persistence::workspace_instructions::RecordTurnInstructionSnapshotOutcome::TurnUnavailable
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// a later complete scan of unchanged source evidence links its
/// candidate to the first registration identity instead of minting another.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn unchanged_source_reuses_its_registered_bundle() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let (first_session, first_turn) = active_instruction_turn(&pool, 0x6400).await?;
    let (second_session, second_turn) = active_instruction_turn(&pool, 0x6500).await?;
    let directory = tempfile::tempdir()?;
    let root_path = directory.path().canonicalize()?;
    std::fs::write(root_path.join("AGENTS.md"), "stable workspace rule\n")?;
    let root = signalbox_domain::InstructionPath::try_new(
        root_path
            .to_str()
            .expect("temporary path is UTF-8")
            .to_owned(),
    )?;
    let snapshot = signalbox_application::discover_workspace_instructions(vec![
        signalbox_application::InstructionDiscoveryRoot::new(
            signalbox_domain::InstructionDiscoveryRootKind::Workspace,
            root,
        ),
    ]);
    let repository =
        signalbox_persistence::workspace_instructions::WorkspaceInstructionRepository::new(
            pool.clone(),
        );
    let first_discovery =
        signalbox_domain::InstructionDiscoveryId::from_uuid(Uuid::from_u128(0x6610));
    let first_manifest_id =
        signalbox_domain::TurnInstructionManifestId::from_uuid(Uuid::from_u128(0x6611));
    let registered_bundle =
        signalbox_domain::InstructionBundleId::from_uuid(Uuid::from_u128(0x6612));
    let first_outcome = repository
        .record_turn_start(
            first_discovery,
            signalbox_domain::TurnInstructionManifest::empty_turn_start(
                first_manifest_id,
                first_session,
                first_turn,
            ),
            &snapshot,
            || registered_bundle,
        )
        .await?;
    let second_discovery =
        signalbox_domain::InstructionDiscoveryId::from_uuid(Uuid::from_u128(0x6620));
    let second_manifest_id =
        signalbox_domain::TurnInstructionManifestId::from_uuid(Uuid::from_u128(0x6621));
    let unused_bundle = signalbox_domain::InstructionBundleId::from_uuid(Uuid::from_u128(0x6622));
    let second_outcome = repository
        .record_turn_start(
            second_discovery,
            signalbox_domain::TurnInstructionManifest::empty_turn_start(
                second_manifest_id,
                second_session,
                second_turn,
            ),
            &snapshot,
            || unused_bundle,
        )
        .await?;
    let second_candidate = sqlx::query_scalar::<_, Uuid>(
        "SELECT instruction_bundle_id
           FROM instruction_discovery_candidate
          WHERE instruction_discovery_id = $1 AND candidate_ordinal = 1",
    )
    .bind(second_discovery.into_uuid())
    .fetch_one(&pool)
    .await?;

    assert_eq!(snapshot.bundles().len(), 1);
    assert_eq!(
        first_outcome,
        signalbox_persistence::workspace_instructions::RecordTurnInstructionSnapshotOutcome::Recorded(
            first_manifest_id,
        )
    );
    assert_eq!(
        second_outcome,
        signalbox_persistence::workspace_instructions::RecordTurnInstructionSnapshotOutcome::Recorded(
            second_manifest_id,
        )
    );
    assert_eq!(second_candidate, registered_bundle.into_uuid());

    pool.close().await;
    drop(container);
    Ok(())
}

/// changing source bytes at a registered path creates a distinct
/// retained registration while preserving the prior bundle identity.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn changed_source_creates_a_distinct_registered_bundle() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let (first_session, first_turn) = active_instruction_turn(&pool, 0x6b00).await?;
    let (second_session, second_turn) = active_instruction_turn(&pool, 0x6c00).await?;
    let directory = tempfile::tempdir()?;
    let root_path = directory.path().canonicalize()?;
    let source_path = root_path.join("AGENTS.md");
    std::fs::write(&source_path, "first workspace rule\n")?;
    let root = signalbox_domain::InstructionPath::try_new(
        root_path
            .to_str()
            .expect("temporary path is UTF-8")
            .to_owned(),
    )?;
    let first_snapshot = signalbox_application::discover_workspace_instructions(vec![
        signalbox_application::InstructionDiscoveryRoot::new(
            signalbox_domain::InstructionDiscoveryRootKind::Workspace,
            root.clone(),
        ),
    ]);
    let repository =
        signalbox_persistence::workspace_instructions::WorkspaceInstructionRepository::new(
            pool.clone(),
        );
    let first_discovery =
        signalbox_domain::InstructionDiscoveryId::from_uuid(Uuid::from_u128(0x6d10));
    let first_manifest_id =
        signalbox_domain::TurnInstructionManifestId::from_uuid(Uuid::from_u128(0x6d11));
    let first_bundle = signalbox_domain::InstructionBundleId::from_uuid(Uuid::from_u128(0x6d12));
    let first_outcome = repository
        .record_turn_start(
            first_discovery,
            signalbox_domain::TurnInstructionManifest::empty_turn_start(
                first_manifest_id,
                first_session,
                first_turn,
            ),
            &first_snapshot,
            || first_bundle,
        )
        .await?;

    std::fs::write(&source_path, "changed workspace rule\n")?;
    let second_snapshot = signalbox_application::discover_workspace_instructions(vec![
        signalbox_application::InstructionDiscoveryRoot::new(
            signalbox_domain::InstructionDiscoveryRootKind::Workspace,
            root,
        ),
    ]);
    let second_discovery =
        signalbox_domain::InstructionDiscoveryId::from_uuid(Uuid::from_u128(0x6e10));
    let second_manifest_id =
        signalbox_domain::TurnInstructionManifestId::from_uuid(Uuid::from_u128(0x6e11));
    let second_bundle = signalbox_domain::InstructionBundleId::from_uuid(Uuid::from_u128(0x6e12));
    let second_outcome = repository
        .record_turn_start(
            second_discovery,
            signalbox_domain::TurnInstructionManifest::empty_turn_start(
                second_manifest_id,
                second_session,
                second_turn,
            ),
            &second_snapshot,
            || second_bundle,
        )
        .await?;
    let first_candidate = sqlx::query_scalar::<_, Uuid>(
        "SELECT instruction_bundle_id
           FROM instruction_discovery_candidate
          WHERE instruction_discovery_id = $1 AND candidate_ordinal = 1",
    )
    .bind(first_discovery.into_uuid())
    .fetch_one(&pool)
    .await?;
    let second_candidate = sqlx::query_scalar::<_, Uuid>(
        "SELECT instruction_bundle_id
           FROM instruction_discovery_candidate
          WHERE instruction_discovery_id = $1 AND candidate_ordinal = 1",
    )
    .bind(second_discovery.into_uuid())
    .fetch_one(&pool)
    .await?;
    let bundle_count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM registered_instruction_bundle WHERE root_path = $1",
    )
    .bind(first_snapshot.roots()[0].path().as_str())
    .fetch_one(&pool)
    .await?;

    assert_eq!(first_snapshot.bundles().len(), 1);
    assert_eq!(second_snapshot.bundles().len(), 1);
    assert_ne!(
        first_snapshot.bundles()[0].source_hash(),
        second_snapshot.bundles()[0].source_hash()
    );
    assert_eq!(
        first_outcome,
        signalbox_persistence::workspace_instructions::RecordTurnInstructionSnapshotOutcome::Recorded(
            first_manifest_id,
        )
    );
    assert_eq!(
        second_outcome,
        signalbox_persistence::workspace_instructions::RecordTurnInstructionSnapshotOutcome::Recorded(
            second_manifest_id,
        )
    );
    assert_eq!(first_candidate, first_bundle.into_uuid());
    assert_eq!(second_candidate, second_bundle.into_uuid());
    assert_ne!(first_candidate, second_candidate);
    assert_eq!(bundle_count, 2);

    pool.close().await;
    drop(container);
    Ok(())
}

/// an incomplete discovery remains durable diagnostic evidence but
/// binds no manifest, so retry can record a later complete snapshot.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn incomplete_discovery_remains_unbound_for_retry() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let (session, turn) = active_instruction_turn(&pool, 0x6200).await?;
    let directory = tempfile::tempdir()?;
    let root_path = directory.path().canonicalize()?;
    let source_path = root_path.join("AGENTS.md");
    std::fs::File::create(&source_path)?.set_len(64 * 1024 * 1024 + 1)?;
    let root = signalbox_domain::InstructionPath::try_new(
        root_path
            .to_str()
            .expect("temporary path is UTF-8")
            .to_owned(),
    )?;
    let incomplete = signalbox_application::discover_workspace_instructions(vec![
        signalbox_application::InstructionDiscoveryRoot::new(
            signalbox_domain::InstructionDiscoveryRootKind::Workspace,
            root.clone(),
        ),
    ]);
    assert!(!incomplete.is_complete());
    let repository =
        signalbox_persistence::workspace_instructions::WorkspaceInstructionRepository::new(
            pool.clone(),
        );
    let first_discovery =
        signalbox_domain::InstructionDiscoveryId::from_uuid(Uuid::from_u128(0x6210));
    let first_manifest_id =
        signalbox_domain::TurnInstructionManifestId::from_uuid(Uuid::from_u128(0x6211));
    let first_manifest = signalbox_domain::TurnInstructionManifest::empty_turn_start(
        first_manifest_id,
        session,
        turn,
    );
    let incomplete_outcome = repository
        .record_turn_start(first_discovery, first_manifest, &incomplete, || {
            panic!("an over-budget source registers no bundle")
        })
        .await?;
    assert_eq!(
        incomplete_outcome,
        signalbox_persistence::workspace_instructions::RecordTurnInstructionSnapshotOutcome::DiscoveryIncomplete
    );
    assert_eq!(
        repository.preflight_turn_start(session, turn).await?,
        signalbox_persistence::workspace_instructions::TurnInstructionManifestPreflight::Absent
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM turn_instruction_manifest WHERE session_id = $1 AND turn_id = $2",
        )
        .bind(session.into_uuid())
        .bind(turn.into_uuid())
        .fetch_one(&pool)
        .await?,
        0
    );
    let persisted_usage = sqlx::query_as::<_, PersistedDiscoveryUsage>(
        "SELECT limit_set_version, classified_entry_count, finding_count,
                candidate_source_byte_count, elapsed_millis, scan_complete
           FROM instruction_discovery
          WHERE instruction_discovery_id = $1",
    )
    .bind(first_discovery.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        persisted_usage.limit_set_version,
        i16::try_from(incomplete.limit_set_version()).expect("fixture limit version fits smallint")
    );
    assert_eq!(
        persisted_usage.classified_entry_count,
        i64::try_from(incomplete.classified_entries()).expect("fixture entry count fits bigint")
    );
    assert_eq!(
        persisted_usage.finding_count,
        i64::try_from(incomplete.findings().len()).expect("fixture finding count fits bigint")
    );
    assert_eq!(
        persisted_usage.candidate_source_byte_count,
        i64::try_from(incomplete.candidate_source_bytes())
            .expect("fixture source byte count fits bigint")
    );
    assert_eq!(
        persisted_usage.elapsed_millis,
        i64::try_from(incomplete.elapsed_millis()).expect("fixture elapsed time fits bigint")
    );
    assert_eq!(persisted_usage.scan_complete, incomplete.is_complete());
    let persisted_roots = sqlx::query_as::<_, PersistedDiscoveryRoot>(
        "SELECT root_ordinal, root_kind, root_path
           FROM instruction_discovery_root
          WHERE instruction_discovery_id = $1
          ORDER BY root_ordinal",
    )
    .bind(first_discovery.into_uuid())
    .fetch_all(&pool)
    .await?;
    let persisted_root = persisted_roots
        .first()
        .expect("the incomplete discovery root is persisted");
    let expected_root = incomplete
        .roots()
        .first()
        .expect("the incomplete discovery names its workspace root");
    assert_eq!(persisted_roots.len(), incomplete.roots().len());
    assert_eq!(persisted_root.root_ordinal, 1);
    assert_eq!(persisted_root.root_kind, "workspace");
    assert_eq!(persisted_root.root_path, expected_root.path().as_str());
    let persisted_findings = sqlx::query_as::<_, PersistedDiscoveryFinding>(
        "SELECT finding_ordinal, source_path, finding_kind
           FROM instruction_discovery_finding
          WHERE instruction_discovery_id = $1
          ORDER BY finding_ordinal",
    )
    .bind(first_discovery.into_uuid())
    .fetch_all(&pool)
    .await?;
    let persisted_finding = persisted_findings
        .first()
        .expect("the incomplete discovery finding is persisted");
    let expected_finding = incomplete
        .findings()
        .first()
        .expect("the incomplete discovery carries its limit finding");
    assert_eq!(persisted_findings.len(), incomplete.findings().len());
    assert_eq!(persisted_finding.finding_ordinal, 1);
    assert_eq!(
        persisted_finding.source_path,
        expected_finding.path().as_str()
    );
    assert_eq!(
        expected_finding.kind(),
        signalbox_application::InstructionDiscoveryFindingKind::LimitReached(
            signalbox_application::InstructionDiscoveryLimitKind::CandidateSourceBytes,
        )
    );
    assert_eq!(
        persisted_finding.finding_kind,
        "limit_candidate_source_bytes"
    );

    std::fs::write(&source_path, "retry rule\n")?;
    let complete = signalbox_application::discover_workspace_instructions(vec![
        signalbox_application::InstructionDiscoveryRoot::new(
            signalbox_domain::InstructionDiscoveryRootKind::Workspace,
            root,
        ),
    ]);
    assert!(complete.is_complete());
    let retry_discovery =
        signalbox_domain::InstructionDiscoveryId::from_uuid(Uuid::from_u128(0x6212));
    let retry_manifest_id =
        signalbox_domain::TurnInstructionManifestId::from_uuid(Uuid::from_u128(0x6213));
    let retry_manifest = signalbox_domain::TurnInstructionManifest::empty_turn_start(
        retry_manifest_id,
        session,
        turn,
    );
    let bundle_id = signalbox_domain::InstructionBundleId::from_uuid(Uuid::from_u128(0x6214));
    let retry_outcome = repository
        .record_turn_start(retry_discovery, retry_manifest, &complete, || bundle_id)
        .await?;
    assert_eq!(
        retry_outcome,
        signalbox_persistence::workspace_instructions::RecordTurnInstructionSnapshotOutcome::Recorded(
            retry_manifest_id,
        )
    );
    assert_eq!(
        repository.preflight_turn_start(session, turn).await?,
        signalbox_persistence::workspace_instructions::TurnInstructionManifestPreflight::Available(
            retry_manifest_id,
        )
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM instruction_discovery WHERE session_id = $1 AND turn_id = $2",
        )
        .bind(session.into_uuid())
        .bind(turn.into_uuid())
        .fetch_one(&pool)
        .await?,
        2
    );

    pool.close().await;
    drop(container);
    Ok(())
}

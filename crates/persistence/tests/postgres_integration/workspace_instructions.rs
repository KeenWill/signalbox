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

#[derive(sqlx::FromRow)]
struct PersistedManifestHashes {
    eligibility_hash: Vec<u8>,
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

#[track_caller]
fn assert_persisted_candidate(
    persisted: &PersistedCandidate,
    ordinal: i64,
    identity: signalbox_domain::InstructionBundleId,
    root_kind: &str,
    bundle_kind: &str,
    expected: &signalbox_domain::InstructionBundleRegistration,
) {
    assert_eq!(persisted.candidate_ordinal, ordinal);
    assert_eq!(persisted.instruction_bundle_id, identity.into_uuid());
    assert_eq!(persisted.root_kind, root_kind);
    assert_eq!(persisted.root_path, expected.root_path().as_str());
    assert_eq!(persisted.source_path, expected.source_path().as_str());
    assert_eq!(persisted.bundle_kind, bundle_kind);
    assert_eq!(
        persisted.skill_name.as_deref(),
        expected
            .skill()
            .map(signalbox_domain::InstructionSkillMetadata::name)
    );
    assert_eq!(
        persisted.skill_description.as_deref(),
        expected
            .skill()
            .map(signalbox_domain::InstructionSkillMetadata::description)
    );
    assert_eq!(
        persisted.source_byte_length,
        Decimal::from(expected.source_bytes())
    );
    assert_eq!(
        persisted.source_hash,
        expected.source_hash().as_bytes().as_slice()
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

async fn active_instruction_turn(pool: &PgPool) -> Result<(SessionId, TurnId), Box<dyn Error>> {
    let session = SessionId::from_uuid(Uuid::from_u128(0x6201));
    let selection = DirectModelSelection::from_uuid(Uuid::from_u128(0x6202));
    let mut create = CreateSessionService::new(
        FixedSessionIds::new([session]),
        CreateSessionRepository::new(pool.clone(), test_session_credential_pin()),
    );
    let CreateSessionOutcome::Applied(_) = create
        .execute(CreateSessionRequest::try_new(
            DurableCommandId::from_uuid(Uuid::from_u128(0x6203)),
            SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(selection)),
        )?)
        .await?
    else {
        panic!("the retry fixture session must be created");
    };
    let turn = TurnId::from_uuid(Uuid::from_u128(0x6204));
    let mut submit = SubmitInputService::new(
        FixedSubmitInputIds::new(
            [AcceptedInputId::from_uuid(Uuid::from_u128(0x6205))],
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
            DurableCommandId::from_uuid(Uuid::from_u128(0x6206)),
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
    let mut activation = StartEligibleTurnService::new(
        FixedStartEligibleTurnIds::new(
            [SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                0x6207,
            ))],
            [ContextFrontierId::from_uuid(Uuid::from_u128(0x6208))],
            [TurnAttemptId::from_uuid(Uuid::from_u128(0x6209))],
        ),
        StartEligibleTurnRepository::new(pool.clone()),
    );
    let StartEligibleTurnOutcome::Activated(activated) = activation.execute(session).await? else {
        panic!("the retry fixture turn must activate");
    };
    assert_eq!(activated.turn(), turn);
    Ok((session, turn))
}

/// INV-061: one active turn records its exact discovery candidates and an
/// immutable empty turn-start instruction manifest before model execution.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv061_turn_instruction_snapshot_is_exact_and_append_only() -> Result<(), Box<dyn Error>> {
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
    let root = signalbox_domain::InstructionPath::try_new(
        root.to_str().expect("temporary path is UTF-8").to_owned(),
    )?;
    let snapshot = signalbox_application::discover_workspace_instructions(vec![
        signalbox_application::InstructionDiscoveryRoot::new(
            signalbox_domain::InstructionDiscoveryRootKind::Workspace,
            root,
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
    let mut bundle_ids = [agent_document_id, agent_skill_id].into_iter();
    let outcome =
        signalbox_persistence::workspace_instructions::WorkspaceInstructionRepository::new(
            pool.clone(),
        )
        .record_turn_start(discovery, manifest.clone(), &snapshot, || {
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
        1,
        agent_document_id,
        "workspace",
        "agent_document",
        snapshot
            .bundles()
            .first()
            .expect("the discovery contains the agent document"),
    );
    assert_persisted_candidate(
        persisted_candidates
            .get(1)
            .expect("the skill candidate is persisted"),
        2,
        agent_skill_id,
        "workspace",
        "agent_skill",
        snapshot
            .bundles()
            .get(1)
            .expect("the discovery contains the skill"),
    );
    let persisted_hashes = sqlx::query_as::<_, PersistedManifestHashes>(
        "SELECT eligibility_hash, manifest_hash
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
    let mutation = sqlx::query(
        "UPDATE turn_instruction_manifest SET boundary_kind = 'turn_start' WHERE turn_instruction_manifest_id = $1",
    )
    .bind(manifest_id.into_uuid())
    .execute(&pool)
    .await;
    assert_append_only_rejection(mutation);
    let deletion = sqlx::query(
        "DELETE FROM turn_instruction_manifest WHERE turn_instruction_manifest_id = $1",
    )
    .bind(manifest_id.into_uuid())
    .execute(&pool)
    .await;
    assert_append_only_rejection(deletion);

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-061: an incomplete discovery remains durable diagnostic evidence but
/// binds no manifest, so retry can record a later complete snapshot.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv061_incomplete_discovery_remains_unbound_for_retry() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let (session, turn) = active_instruction_turn(&pool).await?;
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

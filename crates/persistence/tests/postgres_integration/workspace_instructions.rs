//! Workspace-instruction discovery registration and turn evidence.

use crate::*;

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
    let mut bundle_ids = [Uuid::from_u128(0x6112), Uuid::from_u128(0x6113)].into_iter();
    let outcome =
        signalbox_persistence::workspace_instructions::WorkspaceInstructionRepository::new(
            pool.clone(),
        )
        .record_turn_start(discovery, manifest.clone(), &snapshot, || {
            signalbox_domain::InstructionBundleId::from_uuid(
                bundle_ids
                    .next()
                    .expect("two discovered bundles need two identities"),
            )
        })
        .await?;
    assert_eq!(
        outcome,
        signalbox_persistence::workspace_instructions::RecordTurnInstructionSnapshotOutcome::Recorded(
            manifest_id,
        )
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM instruction_discovery_candidate WHERE instruction_discovery_id = $1",
        )
        .bind(discovery.into_uuid())
        .fetch_one(&pool)
        .await?,
        i64::try_from(snapshot.bundles().len()).expect("fixture bundle count fits PostgreSQL bigint")
    );
    assert_eq!(
        sqlx::query_scalar::<_, Vec<u8>>(
            "SELECT manifest_hash FROM turn_instruction_manifest WHERE turn_instruction_manifest_id = $1",
        )
        .bind(manifest_id.into_uuid())
        .fetch_one(&pool)
        .await?,
        manifest.manifest_hash().as_bytes().as_slice()
    );
    let mutation = sqlx::query(
        "UPDATE turn_instruction_manifest SET boundary_kind = 'turn_start' WHERE turn_instruction_manifest_id = $1",
    )
    .bind(manifest_id.into_uuid())
    .execute(&pool)
    .await;
    assert!(mutation.is_err());

    pool.close().await;
    drop(container);
    Ok(())
}

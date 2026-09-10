//! Partial workspace-instruction scan admission.

use super::*;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn incomplete_discovery_starts_counted_and_uncounted_turns() -> Result<(), Box<dyn Error>> {
    for adapter in ["anthropic", "openai"] {
        let configuration_text = MODEL_CONFIGURATION.replace(
            "adapter = \"anthropic\"",
            &format!("adapter = \"{adapter}\""),
        );
        let runtime = RunningRuntime::start_with_model_configuration(&configuration_text).await?;
        let mut connection = Connection::connect(runtime.socket()).await?;
        let session_id = create_alias_session(&mut connection).await?;
        let (_, turn_id) = submit_first_input(
            &mut connection,
            session_id,
            String::from("Start with the available workspace instructions."),
        )
        .await?;
        let session = SessionId::from_uuid(session_id.into_uuid());
        let directory = tempfile::tempdir()?;
        std::fs::write(
            directory.path().join("AGENTS.md"),
            "Keep the change focused.",
        )?;
        std::fs::create_dir(directory.path().join("child"))?;
        std::fs::write(
            directory.path().join("child/AGENTS.md"),
            "Additional instructions.",
        )?;
        let root = signalbox_domain::InstructionPath::try_new(
            directory
                .path()
                .canonicalize()?
                .to_str()
                .expect("UTF-8 fixture path")
                .to_owned(),
        )?;
        let instructions =
            signalboxd::WorkspaceInstructionRuntime::new(runtime.pool.clone(), None, vec![root])
                .with_discovery_limits(signalbox_application::InstructionDiscoveryLimits {
                    classified_entries: Some(2),
                    ..Default::default()
                });
        let configuration = support::parse_model_configuration(&configuration_text)?;
        let models = configuration.runtime_model_catalog();
        let ordinary = compaction::RecordingCountedScriptedModel::following(
            [completed_script(
                "fixture-model",
                "Task complete.",
                TokenUsage::unreported(),
            )],
            [100],
        );
        let probe = ordinary.clone();
        let provider = RuntimeModelCallProvider::new(ordinary, models.clone(), None);
        let calls = PostgresModelCallRepository::new(
            runtime.pool.clone(),
            configuration.target_catalog(),
            ModelCallCredentialReference::new("partial-discovery-fixture"),
        )
        .with_session_credentials(configuration.credential_family_catalog());
        let execution = signalboxd::WorkspaceInstructionPreparedExecution::new(
            PostgresProviderModelExecution::new(
                calls.clone(),
                InProcessAttemptDispatchGate::default(),
                provider.clone(),
                None,
            ),
            instructions.clone(),
        );
        let mut pass = ContextGuardedTurnPass::new(
            StartEligibleTurnRepository::new(runtime.pool.clone()),
            calls,
            provider,
            NoToolCatalog,
            models.clone(),
            configuration,
            Arc::new(RuntimeContextCompactionModel::new(
                ScriptedModel::following([]),
                models,
            )),
            execution,
        )
        .with_workspace_instructions(instructions);
        pass.run(session).await?;
        assert_eq!(probe.prepared_operations().len(), 1);
        let recorded: (String, String, bool, i64, i64) = sqlx::query_as(
            "SELECT turn.state_kind, attempt.end_disposition, discovery.scan_complete,
                    (SELECT count(*) FROM instruction_discovery_candidate candidate
                      WHERE candidate.instruction_discovery_id = discovery.instruction_discovery_id),
                    (SELECT count(*) FROM instruction_discovery_finding finding
                      WHERE finding.instruction_discovery_id = discovery.instruction_discovery_id
                        AND finding.finding_kind = 'limit_classified_entries')
               FROM turn_lifecycle turn
               JOIN turn_attempt attempt ON attempt.turn_id = turn.turn_id
               JOIN turn_instruction_manifest manifest ON manifest.turn_id = turn.turn_id
               JOIN instruction_discovery discovery USING (instruction_discovery_id)
              WHERE turn.session_id = $1 AND turn.turn_id = $2",
        ).bind(session.into_uuid()).bind(turn_id.into_uuid()).fetch_one(&runtime.pool).await?;
        assert_eq!(
            recorded,
            (
                String::from("terminal"),
                String::from("turn_completed"),
                false,
                1,
                1
            )
        );
        drop(connection);
        runtime.stop().await?;
    }
    Ok(())
}

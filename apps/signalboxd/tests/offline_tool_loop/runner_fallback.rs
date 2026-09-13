use super::*;
use signalbox_domain::{
    CredentialProfileName, RunnerCapabilityClass, RunnerSandboxProfile, RunnerSelector,
    RunnerToolPermissionOverride, RunnerToolPermissionOverrides, RunnerWorkingDirectory,
    SessionRunnerPlacementRequest, SessionRunnerPlacementState, ToolDecisionSource,
    WorkingDirectorySelection, WorkspaceRequirement,
};
use signalboxd::runner_protocol_runtime::PostgresRunnerRegistrationService;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn placed_echo_uses_daemon_permission_without_runner_authority() -> Result<(), Box<dyn Error>>
{
    let named_profile = CredentialProfileName::try_new("github-runner".to_owned())
        .expect("compiled runner profile");
    for credential_profile in [None, Some(named_profile)] {
        let requested = SessionRunnerPlacementRequest {
            selector: RunnerSelector::CapabilityClass(
                RunnerCapabilityClass::try_new("echo".to_owned()).expect("compiled echo class"),
            ),
            working_directory: WorkingDirectorySelection::Exact(
                RunnerWorkingDirectory::try_new("/unavailable/runner/directory".to_owned())
                    .expect("exact runner directory"),
            ),
            credential_profile,
            workspace: WorkspaceRequirement::None,
            sandbox: RunnerSandboxProfile::Ambient,
            permission_overrides: RunnerToolPermissionOverrides::try_new([(
                ToolName::try_new("echo".to_owned()).expect("compiled echo tool"),
                RunnerToolPermissionOverride::Confirm,
            )])
            .expect("one runner permission override"),
        };
        let fixture = ToolLoopFixture::with_creation_placement(
            DangerousToolAutoApproval::Disabled,
            None,
            migrated_postgres().await?,
            Some(requested.clone()),
        )
        .await?;
        let web = OfflineWebTransport::unused();
        let arguments = serde_json::json!({"text": "daemon fallback"}).to_string();
        let (catalog, executor) = offline_daemon_tools(
            web.clone(),
            UnusedSessionStatusWriter,
            UnusedCodeHostTransport,
            WebFetchEgressPolicy::deny_all(),
        )?
        .into_parts();
        let (execution, runtime) = fixture.execution(
            [
                tool_use_script(&[("echo", arguments.as_str())]),
                completion_script("echo observed"),
            ],
            catalog,
            executor,
        );

        execution
            .execute(Box::new(fixture.activated.clone()))
            .await?;

        let request = fixture.wait_for_requests(1).await?[0];
        assert_eq!(
            continuation_tool_exchange(&runtime)?,
            vec![
                expected_tool_call(request, "echo", &arguments),
                expected_successful_tool_result(request, arguments),
            ],
            "placement: {requested:?}",
        );
        let approval = PostgresToolLoopRepository::new(fixture.pool.clone())
            .load_approval(request)
            .await?
            .expect("daemon approval is retained");
        assert_eq!(approval.source(), ToolDecisionSource::PolicyAuto);
        let store = PostgresRunnerRegistrationService::local(fixture.pool.clone())
            .expect("compiled runner catalog")
            .recovery_store();
        let stored = store
            .load_placement(fixture.session)
            .await?
            .expect("created placement");
        assert_eq!(stored.placement().request(), &requested);
        assert_eq!(
            stored.placement().state(),
            &SessionRunnerPlacementState::Unpinned
        );
        assert!(stored.registration().is_none());
        assert!(stored.grant().is_none());
        assert!(web.requests().is_empty());
    }
    Ok(())
}

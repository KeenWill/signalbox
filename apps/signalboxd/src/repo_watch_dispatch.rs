//! Core command adapters for the compiled-in repository-watch module.

use crate::{HubModelConfiguration, SessionTemplateConfiguration};
use signalbox_application::{
    CreateSessionTransaction, EligibilityNudge, InProcessEligibilityNudge,
    InProcessToolDispatchGate,
};
use signalbox_domain::{
    CommandPrincipal, CreateSession, DispatchingModule, DurableCommandId, ModuleDispatch,
    RepoWatchDispatchId, RepoWatchEvent, SessionCreationCause, SessionCreationProvenance,
    SessionId, SessionLifecycleApplication, SessionLifecycleCommand, SessionLifecycleCommandResult,
    SessionLifecycleOperation, SessionTemplateName, TranscriptAncestry,
};
use signalbox_module_repo_watch_v2::{
    CreateSessionCommandFactory, DispatchReferenceGenerator, SessionCommandCodec,
    dispatch::{CommandSubmission, LifecycleCommandFactory, SessionCommandSink},
};
use signalbox_persistence::{
    create_session::CreateSessionRepository,
    repo_watch_command::RepoWatchCommandRecord,
    session_lifecycle_command::{
        SessionLifecycleCommandHandlingOutcome, SessionLifecycleCommandRepository,
    },
};
use signalbox_session_ownership::{SessionCommand, SessionCommandPayload};
use sqlx::PgPool;
use std::{
    ffi::OsString,
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::PathBuf,
    sync::Arc,
};
use uuid::Uuid;

/// Submits retained commands with checkout provisioning inside the held creation.
pub async fn submit_pending(
    store: &signalbox_module_repo_watch_v2::RepoWatchStore,
    configuration: &crate::RepositoryWatchConfiguration,
    sink: &mut RepositoryWatchCommandSink,
) -> Result<
    (),
    signalbox_module_repo_watch_v2::dispatch::SubmissionError<RepositoryWatchCommandError>,
> {
    let runner = sink.checkout_runner.clone();
    submit_with_checkout(store, configuration, sink, runner).await
}

/// Injects the process boundary for deterministic local-repository integration tests.
#[cfg(feature = "test-support")]
pub async fn submit_pending_with_runner<Runner: signalbox_tools_exec::ProcessRunner>(
    store: &signalbox_module_repo_watch_v2::RepoWatchStore,
    configuration: &crate::RepositoryWatchConfiguration,
    sink: &mut RepositoryWatchCommandSink,
    runner: Runner,
) -> Result<
    (),
    signalbox_module_repo_watch_v2::dispatch::SubmissionError<RepositoryWatchCommandError>,
> {
    submit_with_checkout(store, configuration, sink, Some(runner)).await
}

async fn submit_with_checkout<Runner: signalbox_tools_exec::ProcessRunner>(
    store: &signalbox_module_repo_watch_v2::RepoWatchStore,
    configuration: &crate::RepositoryWatchConfiguration,
    sink: &mut RepositoryWatchCommandSink,
    runner: Option<Runner>,
) -> Result<
    (),
    signalbox_module_repo_watch_v2::dispatch::SubmissionError<RepositoryWatchCommandError>,
> {
    let source = signalbox_session_ownership::LifecycleEventSource::new(sink.pool.clone());
    store
        .submit_pending(
            &mut RepositoryWatchCommandCodec,
            &mut CheckoutCommandSink {
                store,
                configuration,
                core: sink,
                runner,
            },
            &source,
        )
        .await
}

/// Removes retired dispatch checkouts during startup and lifecycle processing.
pub async fn scavenge_checkouts(
    store: &signalbox_module_repo_watch_v2::RepoWatchStore,
    core: &PgPool,
) -> Result<(), RepositoryWatchCommandError> {
    let checkouts = store
        .checkout_removal_candidates()
        .await
        .map_err(|_| RepositoryWatchCommandError::CoreCommandFailed)?;
    let sessions: Vec<Uuid> = checkouts
        .iter()
        .filter(|checkout| checkout.retired_reason.is_none())
        .map(|checkout| checkout.location.session.into_uuid())
        .collect();
    let terminal: std::collections::BTreeSet<Uuid> = if sessions.is_empty() {
        Default::default()
    } else {
        sqlx::query_scalar(
            "SELECT session_id FROM session_lifecycle WHERE session_id = ANY($1) AND state_kind = 'terminal'",
        ).bind(&sessions).fetch_all(core)
        .await
        .map_err(|_| RepositoryWatchCommandError::CoreCommandFailed)?
        .into_iter().collect()
    };
    for checkout in checkouts {
        let session = checkout.location.session;
        if checkout.retired_reason.is_none() && !terminal.contains(&session.into_uuid()) {
            continue;
        }
        let workspace_root = PathBuf::from(OsString::from_vec(checkout.location.workspace_root));
        let Ok(roots) = crate::daemon_tools::SessionWorkspaceRoots::try_new(&workspace_root) else {
            tracing::warn!(
                reason = "checkout_workspace_root_rejected",
                "repository-watch checkout removal skipped"
            );
            continue;
        };
        let removal = tokio::task::spawn_blocking(move || {
            crate::repo_watch_checkout::remove(
                &roots,
                session,
                checkout.dispatch,
                checkout.created,
                checkout.identity,
            )
        })
        .await;
        if !matches!(removal, Ok(Ok(()))) {
            tracing::warn!(
                reason = "checkout_removal_failed",
                "repository-watch checkout removal failed"
            );
            continue;
        }
        store
            .settle_checkout_removal(checkout.command)
            .await
            .map_err(|_| RepositoryWatchCommandError::CoreCommandFailed)?;
    }
    Ok(())
}

struct CheckoutCommandSink<'a, Runner> {
    store: &'a signalbox_module_repo_watch_v2::RepoWatchStore,
    configuration: &'a crate::RepositoryWatchConfiguration,
    core: &'a mut RepositoryWatchCommandSink,
    runner: Option<Runner>,
}

impl<Runner: signalbox_tools_exec::ProcessRunner> SessionCommandSink
    for CheckoutCommandSink<'_, Runner>
{
    type Error = RepositoryWatchCommandError;

    async fn submit(&mut self, command: SessionCommand) -> Result<CommandSubmission, Self::Error> {
        use crate::repo_watch_checkout::{CheckoutProvisioningFailed, CheckoutStep};
        use signalbox_application::CreateSessionOutcome;
        use signalbox_domain::{DescendantTerminationScope, RepoWatchEventTarget, StopStickiness};
        use signalbox_module_repo_watch_v2::checkout::CheckoutRetirementReason;

        let id = command.command_id();
        let checkout = self
            .store
            .dispatch_checkout(id)
            .await
            .map_err(|_| RepositoryWatchCommandError::CoreCommandFailed)?;
        let checkout_provisioning_pending = checkout.as_ref().is_some_and(|checkout| {
            matches!(
                checkout.event.target(),
                RepoWatchEventTarget::PullRequest(_)
            )
        });
        let result = self
            .core
            .submit_with_checkout_provisioning(command, checkout_provisioning_pending)
            .await?;
        let CommandSubmission::Creation(CreateSessionOutcome::Applied(applied)) = &result else {
            return Ok(result);
        };
        let Some(checkout) = checkout else {
            return Ok(result);
        };
        let RepoWatchEventTarget::PullRequest(context) = checkout.event.target() else {
            return Ok(result);
        };
        let session = applied.session();
        let stop = if let Some(stop) = checkout.stop_command {
            let sticky = match checkout.retired_reason {
                Some(CheckoutRetirementReason::RepositoryUnconfigured) => {
                    StopStickiness::Redispatchable
                }
                _ => StopStickiness::Sticky,
            };
            Some((stop, sticky))
        } else if checkout.removed || checkout.head.as_ref() == Some(context.head_sha()) {
            None
        } else if let Some(repository) = self
            .configuration
            .repositories()
            .iter()
            .find(|repository| repository.repository() == checkout.event.repository())
        {
            let location = match checkout.location {
                Some(location) => Some(location),
                None => match self.core.models.daemon_tools() {
                    Some(tools) => Some(
                        self.store
                            .retain_checkout_location(
                                id,
                                session,
                                tools.workspace_root().as_os_str().as_bytes(),
                            )
                            .await
                            .map_err(|_| RepositoryWatchCommandError::CoreCommandFailed)?,
                    ),
                    None => None,
                },
            };
            let prepared = (|| {
                let location =
                    location.ok_or(CheckoutProvisioningFailed::at(CheckoutStep::Configuration))?;
                let workspace_root = PathBuf::from(OsString::from_vec(location.workspace_root));
                let roots = crate::daemon_tools::SessionWorkspaceRoots::try_new(&workspace_root)
                    .map_err(|_| CheckoutProvisioningFailed::at(CheckoutStep::Workspace))?;
                crate::repo_watch_checkout::prepare(&roots, location.session, checkout.dispatch)
            })();
            let provisioned = match prepared {
                Ok(mut directory) => {
                    let retained = self
                        .store
                        .retain_checkout_identity(id, directory.identity, directory.created)
                        .await
                        .map_err(|_| RepositoryWatchCommandError::CoreCommandFailed)?;
                    if retained != Some(directory.identity) {
                        Err(CheckoutProvisioningFailed::at(CheckoutStep::Workspace))
                    } else if let Some(runner) = self.runner.as_mut() {
                        crate::repo_watch_checkout::provision(
                            runner,
                            &mut directory,
                            checkout.event.repository(),
                            context,
                            &crate::repo_watch_credentials::RepositoryWatchClientLoader::new(
                                repository,
                            ),
                        )
                        .await
                    } else {
                        Err(CheckoutProvisioningFailed::at(CheckoutStep::Configuration))
                    }
                }
                Err(failure) => Err(failure),
            };
            match provisioned {
                Ok(()) => {
                    self.store
                        .record_dispatch_checkout(id, context.head_sha())
                        .await
                        .map_err(|_| RepositoryWatchCommandError::CoreCommandFailed)?;
                    None
                }
                Err(failure) => {
                    tracing::warn!(reason = "checkout_provisioning_failed", step = failure.step.as_str(), status = %failure.status(), ?session,
                        "repository-watch checkout provisioning failed");
                    Some((
                        self.store
                            .retire_dispatch_checkout(
                                id,
                                CheckoutRetirementReason::ProvisioningFailed,
                                failure.step.as_str(),
                                &failure.status(),
                                DurableCommandId::from_uuid(Uuid::now_v7()),
                            )
                            .await
                            .map_err(|_| RepositoryWatchCommandError::CoreCommandFailed)?,
                        StopStickiness::Sticky,
                    ))
                }
            }
        } else {
            tracing::warn!(
                reason = "repository_unconfigured",
                ?session,
                "repository-watch dispatch retired"
            );
            Some((
                self.store
                    .retire_dispatch_checkout(
                        id,
                        CheckoutRetirementReason::RepositoryUnconfigured,
                        CheckoutStep::Configuration.as_str(),
                        "not_started",
                        DurableCommandId::from_uuid(Uuid::now_v7()),
                    )
                    .await
                    .map_err(|_| RepositoryWatchCommandError::CoreCommandFailed)?,
                StopStickiness::Redispatchable,
            ))
        };
        if let Some((stop, sticky)) = stop {
            let stop = SessionLifecycleCommand::new(
                stop,
                session,
                SessionLifecycleOperation::Stop {
                    sticky,
                    descendant_scope: DescendantTerminationScope::ParentAlone,
                },
            );
            if !matches!(
                self.core.submit_lifecycle(stop).await?,
                CommandSubmission::Accepted
            ) {
                return Err(RepositoryWatchCommandError::CoreCommandFailed);
            }
        } else if !checkout.removed {
            use signalbox_module_repo_watch_v2::checkout::KickoffPushAuthority;
            let push_authority = match crate::repo_watch_runtime::git_push_repository(
                self.configuration,
                &checkout.event,
            ) {
                Some(_) => KickoffPushAuthority::Available,
                None => KickoffPushAuthority::Unavailable,
            };
            if let Some((kickoff, text)) = self
                .store
                .retain_dispatch_kickoff(
                    id,
                    DurableCommandId::from_uuid(Uuid::now_v7()),
                    push_authority,
                )
                .await
                .map_err(|_| RepositoryWatchCommandError::CoreCommandFailed)?
            {
                match self.core.submit_kickoff(kickoff, session, text).await {
                    Err(RepositoryWatchCommandError::KickoffRejected) => {
                        let (retry, text) = self
                            .store
                            .retry_dispatch_kickoff(
                                id,
                                kickoff,
                                DurableCommandId::from_uuid(Uuid::now_v7()),
                            )
                            .await
                            .map_err(|_| RepositoryWatchCommandError::CoreCommandFailed)?
                            .ok_or(RepositoryWatchCommandError::CoreCommandFailed)?;
                        self.core.submit_kickoff(retry, session, text).await?;
                    }
                    result => result?,
                }
            }
            signalbox_persistence::start_eligible_turn::StartEligibleTurnRepository::new(
                self.core.pool.clone(),
            )
            .complete_checkout_provisioning(session)
            .await
            .map_err(|_| RepositoryWatchCommandError::CoreCommandFailed)?;
            let release = SessionLifecycleCommand::new(
                DurableCommandId::from_uuid(Uuid::now_v7()),
                session,
                SessionLifecycleOperation::ReleaseStart,
            );
            if !matches!(
                self.core.submit_lifecycle(release).await?,
                CommandSubmission::Accepted
            ) {
                return Err(RepositoryWatchCommandError::CoreCommandFailed);
            }
            let _ = self.core.eligibility_nudge.nudge(session);
        }
        Ok(result)
    }
}

/// Reserves core command identities and copies the resolved template into each creation.
#[derive(Clone)]
pub struct RepositoryWatchCommandFactory(pub Arc<SessionTemplateConfiguration>);

impl CreateSessionCommandFactory for RepositoryWatchCommandFactory {
    type Error = RepositoryWatchCommandError;
    fn create_session(
        &mut self,
        dispatch: RepoWatchDispatchId,
        template: &SessionTemplateName,
        _: &RepoWatchEvent,
    ) -> Result<CreateSession, Self::Error> {
        let template = self
            .0
            .resolve(template)
            .ok_or(RepositoryWatchCommandError::TemplateUnavailable)?;
        Ok(CreateSession::new_from_template(
            DurableCommandId::from_uuid(Uuid::now_v7()),
            SessionCreationProvenance::new(
                SessionCreationCause::ModuleDispatched {
                    dispatch: ModuleDispatch::RepositoryWatch { dispatch },
                },
                TranscriptAncestry::None,
            ),
            template.provenance().clone(),
            template.defaults().clone(),
        ))
    }
}
impl LifecycleCommandFactory for RepositoryWatchCommandFactory {
    fn lifecycle(
        &mut self,
        session: SessionId,
        operation: SessionLifecycleOperation,
    ) -> SessionLifecycleCommand {
        SessionLifecycleCommand::new(
            DurableCommandId::from_uuid(Uuid::now_v7()),
            session,
            operation,
        )
    }
}

/// Module-owned dispatch identities are independent of core command identities.
pub struct RepositoryWatchDispatchIds;
impl DispatchReferenceGenerator for RepositoryWatchDispatchIds {
    fn next_dispatch(&mut self) -> RepoWatchDispatchId {
        RepoWatchDispatchId::from_uuid(Uuid::now_v7())
    }
}

/// Delegates exact durable payload representation to the core persistence boundary.
pub struct RepositoryWatchCommandCodec;
impl SessionCommandCodec for RepositoryWatchCommandCodec {
    fn encode(&mut self, command: &SessionCommand) -> Option<Vec<u8>> {
        match command.clone().into_payload() {
            SessionCommandPayload::CreateSession(command) => {
                RepoWatchCommandRecord::Create(Box::new(command)).encode()
            }
            SessionCommandPayload::Lifecycle(command) => {
                RepoWatchCommandRecord::Lifecycle(command).encode()
            }
            _ => None,
        }
    }
    fn decode(&mut self, payload: &[u8]) -> Option<SessionCommand> {
        match RepoWatchCommandRecord::decode(payload)? {
            RepoWatchCommandRecord::Create(command) => {
                SessionCommand::create_session(*command).ok()
            }
            RepoWatchCommandRecord::Lifecycle(command) => SessionCommand::lifecycle(command).ok(),
        }
    }
}

/// Applies seam commands through the ordinary core handlers and interrupt machinery.
pub struct RepositoryWatchCommandSink {
    pub checkout_runner: Option<signalbox_tools_exec::TokioProcessRunner>,
    pub pool: PgPool,
    pub models: Arc<HubModelConfiguration>,
    pub eligibility_nudge: InProcessEligibilityNudge,
    pub tool_dispatch_gate: InProcessToolDispatchGate,
}

/// Closed adapter failures, without command payloads or credentials.
#[derive(Debug)]
pub enum RepositoryWatchCommandError {
    TemplateUnavailable,
    UnsupportedCommand,
    CoreCommandFailed,
    KickoffRejected,
    InterruptFailed,
    CheckoutRemovalFailed,
}

impl SessionCommandSink for RepositoryWatchCommandSink {
    type Error = RepositoryWatchCommandError;
    async fn submit(&mut self, command: SessionCommand) -> Result<CommandSubmission, Self::Error> {
        self.submit_with_checkout_provisioning(command, false).await
    }
}

impl RepositoryWatchCommandSink {
    async fn submit_kickoff(
        &mut self,
        command: DurableCommandId,
        session: SessionId,
        text: String,
    ) -> Result<(), RepositoryWatchCommandError> {
        use signalbox_application::{
            SubmitInputOutcome, SubmitInputRequest, SubmitInputService,
            UuidV7SubmitInputIdGenerator,
        };
        use signalbox_domain::{
            DeliveryRequest, ModelSelectionOverride, ParentTerminationKind,
            PerInputConfigurationChoices, SubmitInputResult, UserContent,
        };
        let repository =
            signalbox_persistence::submit_input::SubmitInputRepository::with_model_capabilities(
                self.pool.clone(),
                self.models.model_capability_catalog(),
            );
        let delivery = match repository
            .load(command)
            .await
            .map_err(|_| RepositoryWatchCommandError::CoreCommandFailed)?
        {
            Some(recorded) => recorded.command().delivery(),
            None => {
                let current =
                    signalbox_persistence::session::SessionRepository::new(self.pool.clone())
                        .load_session(session)
                        .await
                        .map_err(|_| RepositoryWatchCommandError::CoreCommandFailed)?
                        .ok_or(RepositoryWatchCommandError::CoreCommandFailed)?;
                DeliveryRequest::StartWhenNoActiveTurn {
                    configuration: PerInputConfigurationChoices::new(
                        current.current_configuration_defaults().version(),
                        ModelSelectionOverride::UseSessionDefault,
                    ),
                }
            }
        };
        let request = SubmitInputRequest::try_new(
            command,
            session,
            UserContent::try_text(text)
                .map_err(|_| RepositoryWatchCommandError::UnsupportedCommand)?,
            delivery,
        )
        .map_err(|_| RepositoryWatchCommandError::UnsupportedCommand)?;
        let mut service = SubmitInputService::new(
            UuidV7SubmitInputIdGenerator,
            crate::process_runtime::ConfiguredSubmitInputTransaction {
                repository,
                model_configuration: &self.models,
                principal: CommandPrincipal::Module {
                    module: DispatchingModule::RepositoryWatch,
                },
                cascade_root_kind: ParentTerminationKind::Cancelled,
            },
            self.eligibility_nudge.clone(),
            self.tool_dispatch_gate.clone(),
        );
        match service.execute(request).await {
            Ok(SubmitInputOutcome::Recorded(SubmitInputResult::Applied(_))) => Ok(()),
            Ok(SubmitInputOutcome::Recorded(SubmitInputResult::Rejected(_))) => {
                Err(RepositoryWatchCommandError::KickoffRejected)
            }
            Ok(SubmitInputOutcome::ConflictingReuse { .. }) => {
                Err(RepositoryWatchCommandError::CoreCommandFailed)
            }
            Err(error) => {
                tracing::warn!(?session, %error, "repository-watch kickoff submission failed");
                Err(RepositoryWatchCommandError::CoreCommandFailed)
            }
        }
    }

    async fn submit_with_checkout_provisioning(
        &mut self,
        command: SessionCommand,
        checkout_provisioning_pending: bool,
    ) -> Result<CommandSubmission, RepositoryWatchCommandError> {
        match command.into_payload() {
            SessionCommandPayload::CreateSession(command) => {
                let prepared = command
                    .prepare(SessionId::from_uuid(Uuid::now_v7()))
                    .map_err(|_| RepositoryWatchCommandError::UnsupportedCommand)?;
                let mut repository = CreateSessionRepository::new(
                    self.pool.clone(),
                    self.models.session_credential_pin(),
                )
                .with_principal(CommandPrincipal::Module {
                    module: DispatchingModule::RepositoryWatch,
                });
                if checkout_provisioning_pending {
                    repository = repository.with_checkout_provisioning();
                }
                let outcome = CreateSessionTransaction::handle(&mut repository, prepared)
                    .await
                    .map_err(|_| RepositoryWatchCommandError::CoreCommandFailed)?;
                Ok(CommandSubmission::Creation(outcome))
            }
            SessionCommandPayload::Lifecycle(command) => self.submit_lifecycle(command).await,
            _ => Err(RepositoryWatchCommandError::UnsupportedCommand),
        }
    }
}

impl RepositoryWatchCommandSink {
    // Daemon checkout disposition uses the normal lifecycle handler; module-issued
    // commands still pass through the ownership seam's closed admission.
    async fn submit_lifecycle(
        &mut self,
        command: SessionLifecycleCommand,
    ) -> Result<CommandSubmission, RepositoryWatchCommandError> {
        let outcome = SessionLifecycleCommandRepository::new(self.pool.clone())
            .handle(
                command.clone(),
                CommandPrincipal::Module {
                    module: DispatchingModule::RepositoryWatch,
                },
            )
            .await
            .map_err(|_| RepositoryWatchCommandError::CoreCommandFailed)?;
        match outcome {
            SessionLifecycleCommandHandlingOutcome::ConflictingReuse { .. } => {
                Ok(CommandSubmission::ConflictingReuse)
            }
            SessionLifecycleCommandHandlingOutcome::Recorded(result) => {
                if let SessionLifecycleCommandResult::Applied(application) = result {
                    match application {
                        SessionLifecycleApplication::StartReleased => {
                            let _ = self.eligibility_nudge.nudge(command.session());
                        }
                        SessionLifecycleApplication::ClosurePending {
                            live_turn,
                            defaults_version,
                            ..
                        } => {
                            crate::process_runtime::interrupt_for_committed_closure(
                                &self.pool,
                                &self.models,
                                &self.eligibility_nudge,
                                &self.tool_dispatch_gate,
                                &command,
                                live_turn,
                                defaults_version,
                            )
                            .await
                            .map_err(|_| RepositoryWatchCommandError::InterruptFailed)?;
                        }
                        _ => {}
                    }
                }
                Ok(CommandSubmission::Accepted)
            }
        }
    }
}

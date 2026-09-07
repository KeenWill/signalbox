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
use signalbox_ownership_seam::{SessionCommand, SessionCommandPayload};
use signalbox_persistence::{
    create_session::CreateSessionRepository,
    repo_watch_command::RepoWatchCommandRecord,
    session_lifecycle_command::{
        SessionLifecycleCommandHandlingOutcome, SessionLifecycleCommandRepository,
    },
};
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

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
    InterruptFailed,
}

impl SessionCommandSink for RepositoryWatchCommandSink {
    type Error = RepositoryWatchCommandError;
    async fn submit(&mut self, command: SessionCommand) -> Result<CommandSubmission, Self::Error> {
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
                let outcome = CreateSessionTransaction::handle(&mut repository, prepared)
                    .await
                    .map_err(|_| RepositoryWatchCommandError::CoreCommandFailed)?;
                Ok(CommandSubmission::Creation(outcome))
            }
            SessionCommandPayload::Lifecycle(command) => {
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
            _ => Err(RepositoryWatchCommandError::UnsupportedCommand),
        }
    }
}

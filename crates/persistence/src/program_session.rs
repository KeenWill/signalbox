//! Host-side session input and durable turn outcomes for registered programs.

use signalbox_domain::{
    AcceptedInputId, CancelledModelCallTurnIdentities, ContextFrontierId, DeliveryRequest,
    FrozenAliasDefinition, ModelAlias, ProgramRunId, SemanticTranscriptEntryId, SessionId,
    SubmitInput, SubmitInputAppliedResult, SubmitInputResult, TurnId,
    program_registration::ProgramContentDigest,
    program_session::{
        ProgramSessionCreate, ProgramSessionDisposition, ProgramSessionOutcome, ProgramSessionTurn,
    },
};
use sqlx::{
    PgPool, Row,
    postgres::{PgListener, PgPoolOptions},
};
use std::sync::Arc;
use tokio::sync::{Mutex, watch};
use uuid::Uuid;

use crate::{
    mapping::{
        TurnDispositionStorageKind, turn_disposition_kind_from_str, turn_disposition_kind_to_str,
    },
    program_journal::{
        ProgramJournalRepository, ProgramJournalRepositoryError, ProgramSessionHost,
    },
    submit_input::{SubmitInputHandlingOutcome, SubmitInputRepository, SubmitInputRepositoryError},
};

#[derive(Debug, signalbox_derive::OperatorError)]
pub enum ProgramSessionError {
    #[error(transparent)]
    Create(#[source] crate::create_session::CreateSessionRepositoryError),
    #[error("invalid program session command identity")]
    InvalidCommand,
    #[error(transparent)]
    Database(#[source] sqlx::Error),
    #[error(transparent)]
    Listener(#[source] Arc<sqlx::Error>),
    #[error(transparent)]
    Journal(#[source] ProgramJournalRepositoryError),
    #[error(transparent)]
    Capability(#[source] crate::program_journal::ProgramSessionCapabilityError),
    #[error(transparent)]
    Submit(#[source] SubmitInputRepositoryError),
    #[error(transparent)]
    Admission(#[source] signalbox_application::SubmitInputRequestError),
    #[error("program session grant denied")]
    GrantDenied,
    #[error("program run terminated")]
    RunEnded,
    #[error("program session input refused")]
    Refused,
    #[error("program session command identity conflict")]
    Conflict,
    #[error("program session corruption: {field_0}")]
    Corruption(&'static str),
}

impl From<crate::create_session::CreateSessionRepositoryError> for ProgramSessionError {
    fn from(error: crate::create_session::CreateSessionRepositoryError) -> Self {
        Self::Create(error)
    }
}

impl From<sqlx::Error> for ProgramSessionError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

impl From<ProgramJournalRepositoryError> for ProgramSessionError {
    fn from(error: ProgramJournalRepositoryError) -> Self {
        Self::Journal(error)
    }
}

impl From<crate::program_journal::ProgramSessionCapabilityError> for ProgramSessionError {
    fn from(error: crate::program_journal::ProgramSessionCapabilityError) -> Self {
        Self::Capability(error)
    }
}

impl From<SubmitInputRepositoryError> for ProgramSessionError {
    fn from(error: SubmitInputRepositoryError) -> Self {
        Self::Submit(error)
    }
}

impl From<signalbox_application::SubmitInputRequestError> for ProgramSessionError {
    fn from(error: signalbox_application::SubmitInputRequestError) -> Self {
        Self::Admission(error)
    }
}

type SessionActivity = watch::Receiver<Option<Arc<sqlx::Error>>>;

#[derive(Clone, Debug)]
pub struct ProgramSessionRepository {
    pool: PgPool,
    input: SubmitInputRepository,
    creation: crate::create_session::CreateSessionRepository,
    activity: Arc<Mutex<Option<SessionActivity>>>,
}

impl ProgramSessionRepository {
    pub fn new(
        pool: PgPool,
        input: SubmitInputRepository,
        creation: crate::create_session::CreateSessionRepository,
    ) -> Self {
        Self {
            pool,
            input,
            creation,
            activity: Arc::new(Mutex::new(None)),
        }
    }

    async fn creation_command(
        &self,
        run: ProgramRunId,
        input: ProgramSessionCreate,
    ) -> Result<signalbox_domain::CreateSession, ProgramSessionError> {
        signalbox_application::CreateSessionRequest::try_new(input.command, input.defaults.clone())
            .map_err(|_| ProgramSessionError::InvalidCommand)?;
        let capability = ProgramSessionHost::new(ProgramJournalRepository::new(self.pool.clone()))
            .session_capability(run)
            .await?
            .ok_or(ProgramSessionError::GrantDenied)?;
        Ok(signalbox_domain::CreateSession::new(
            input.command,
            signalbox_domain::SessionCreationProvenance::workflow(capability),
            input.defaults,
        ))
    }

    /// Creates a workflow session through the ordinary durable creation transaction.
    pub async fn create(
        &self,
        run: ProgramRunId,
        input: ProgramSessionCreate,
    ) -> Result<SessionId, ProgramSessionError> {
        let command = self.creation_command(run, input).await?;
        let prepared = command
            .prepare(SessionId::from_uuid(Uuid::now_v7()))
            .map_err(|_| ProgramSessionError::Corruption("workflow preparation"))?;
        match self.creation.handle(prepared).await? {
            crate::create_session::CreateSessionHandlingOutcome::Applied(result) => {
                Ok(result.session())
            }
            crate::create_session::CreateSessionHandlingOutcome::ConflictingReuse { .. } => {
                Err(ProgramSessionError::Conflict)
            }
        }
    }

    /// Adopts only the receipt for the same program, command identity, and defaults.
    pub async fn adopt_creation(
        &self,
        run: ProgramRunId,
        input: ProgramSessionCreate,
    ) -> Result<Option<SessionId>, ProgramSessionError> {
        let command = self.creation_command(run, input).await?;
        let Some(recorded) = self.creation.load(command.command_id()).await? else {
            return Ok(None);
        };
        if recorded.command() != &command {
            return Err(ProgramSessionError::Conflict);
        }
        Ok(Some(recorded.applied_result().session()))
    }

    async fn command(
        &self,
        run: ProgramRunId,
        input: ProgramSessionTurn,
    ) -> Result<SubmitInput, ProgramSessionError> {
        let capability = ProgramSessionHost::new(ProgramJournalRepository::new(self.pool.clone()))
            .session_capability(run)
            .await?
            .ok_or(ProgramSessionError::GrantDenied)?;
        let admitted = signalbox_application::SubmitInputRequest::try_new_program(
            input.command,
            input.session,
            input.content,
            DeliveryRequest::StartWhenNoActiveTurn {
                configuration: input.configuration,
            },
            capability,
        )?;
        Ok(SubmitInput::new_program(
            admitted.command_id(),
            admitted.session(),
            admitted.content().clone(),
            admitted.delivery(),
            capability.reference(),
        ))
    }

    /// Submits one ordinary turn using the existing program admissibility path, then awaits it.
    pub async fn drive_turn(
        &self,
        run: ProgramRunId,
        input: ProgramSessionTurn,
        select_definition: impl FnOnce(ModelAlias) -> Option<FrozenAliasDefinition>,
        nudge: impl Fn(SessionId),
    ) -> Result<ProgramSessionOutcome, ProgramSessionError> {
        let command = self.command(run, input).await?;
        let result = self
            .input
            .handle_with_candidates_alias_resolver(
                command,
                AcceptedInputId::from_uuid(Uuid::now_v7()),
                Some(TurnId::from_uuid(Uuid::now_v7())),
                CancelledModelCallTurnIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                    ContextFrontierId::from_uuid(Uuid::now_v7()),
                ),
                |_| TurnId::from_uuid(Uuid::now_v7()),
                |requests| {
                    (
                        requests
                            .iter()
                            .map(|_| SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()))
                            .collect(),
                        ContextFrontierId::from_uuid(Uuid::now_v7()),
                    )
                },
                select_definition,
            )
            .await?;
        match result {
            SubmitInputHandlingOutcome::Recorded(result) => {
                self.await_result(run, result, nudge).await
            }
            SubmitInputHandlingOutcome::ConflictingReuse { .. } => {
                Err(ProgramSessionError::Conflict)
            }
        }
    }

    /// Recovers the exact receipt before following its turn; no input is resubmitted.
    pub async fn adopt_turn(
        &self,
        run: ProgramRunId,
        input: ProgramSessionTurn,
        nudge: impl Fn(SessionId),
    ) -> Result<Option<ProgramSessionOutcome>, ProgramSessionError> {
        let command = self.command(run, input).await?;
        let Some(recorded) = self.input.load(command.command_id()).await? else {
            return Ok(None);
        };
        if recorded.command() != &command {
            return Err(ProgramSessionError::Conflict);
        }
        self.await_result(run, recorded.result().clone(), nudge)
            .await
            .map(Some)
    }

    async fn subscribe(&self) -> Result<SessionActivity, sqlx::Error> {
        let mut activity = self.activity.lock().await;
        if let Some(receiver) = activity
            .as_ref()
            .filter(|receiver| receiver.has_changed().is_ok())
        {
            return Ok(receiver.clone());
        }
        let listener_pool = PgPoolOptions::new()
            .max_connections(1)
            .max_lifetime(None)
            .idle_timeout(None)
            .connect_lazy_with(self.pool.connect_options().as_ref().clone());
        let mut listener = PgListener::connect_with(&listener_pool).await?;
        listener.listen("program_session_activity").await?;
        let (notifications, receiver) = watch::channel(None);
        let query_pool = self.pool.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = notifications.closed() => break,
                    _ = query_pool.close_event() => break,
                    result = listener.try_recv() => {
                        let error = result.err().map(Arc::new);
                        let failed = error.is_some();
                        notifications.send_replace(error);
                        if failed {
                            break;
                        }
                    }
                }
            }
        });
        *activity = Some(receiver.clone());
        Ok(receiver)
    }

    async fn await_result(
        &self,
        run: ProgramRunId,
        result: SubmitInputResult,
        nudge: impl Fn(SessionId),
    ) -> Result<ProgramSessionOutcome, ProgramSessionError> {
        let SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(origin)) = result
        else {
            return Err(ProgramSessionError::Refused);
        };
        nudge(origin.session());
        let mut activity = self.subscribe().await?;
        loop {
            let journal = ProgramJournalRepository::new(self.pool.clone())
                .load(run)
                .await?
                .ok_or(ProgramSessionError::Corruption("run journal"))?;
            if journal.terminal_delivery().is_some() {
                return Err(ProgramSessionError::RunEnded);
            }
            if let Some(outcome) = self
                .outcome(origin.session(), origin.turn(), origin.accepted_input())
                .await?
            {
                return Ok(outcome);
            }
            activity
                .changed()
                .await
                .map_err(|_| sqlx::Error::PoolClosed)?;
            if let Some(error) = activity.borrow_and_update().clone() {
                return Err(ProgramSessionError::Listener(error));
            }
        }
    }

    async fn outcome(
        &self,
        session: SessionId,
        turn: TurnId,
        accepted_input: AcceptedInputId,
    ) -> Result<Option<ProgramSessionOutcome>, ProgramSessionError> {
        let row = sqlx::query(
            "SELECT turn.state_kind, turn.terminal_disposition_kind,
            turn.terminal_frontier_id, turn.session_id, turn.origin_accepted_input_id,
            input.session_id AS input_session_id, input.origin_turn_id
            FROM turn_lifecycle AS turn JOIN accepted_input AS input ON input.accepted_input_id = $2
            WHERE turn.turn_id = $1",
        )
        .bind(turn.into_uuid())
        .bind(accepted_input.into_uuid())
        .fetch_optional(&self.pool)
        .await?
        .ok_or(ProgramSessionError::Corruption("turn origin"))?;
        if row.try_get::<Uuid, _>("session_id")? != session.into_uuid()
            || row.try_get::<Uuid, _>("input_session_id")? != session.into_uuid()
            || row.try_get::<Option<Uuid>, _>("origin_turn_id")? != Some(turn.into_uuid())
            || row.try_get::<Option<Uuid>, _>("origin_accepted_input_id")?
                != Some(accepted_input.into_uuid())
        {
            return Err(ProgramSessionError::Corruption("turn origin correlation"));
        }
        let state: String = row.try_get("state_kind")?;
        match state.as_str() {
            "queued" | "active" => return Ok(None),
            "terminal" => {}
            _ => return Err(ProgramSessionError::Corruption("turn state")),
        }
        let stored: String = row
            .try_get::<Option<String>, _>("terminal_disposition_kind")?
            .ok_or(ProgramSessionError::Corruption("terminal disposition"))?;
        let kind = turn_disposition_kind_from_str(&stored)
            .ok_or(ProgramSessionError::Corruption("turn disposition"))?;
        let disposition = match kind {
            TurnDispositionStorageKind::Completed => ProgramSessionDisposition::Completed,
            TurnDispositionStorageKind::Refused => ProgramSessionDisposition::Refused,
            TurnDispositionStorageKind::Failed => ProgramSessionDisposition::Failed,
            TurnDispositionStorageKind::Cancelled => ProgramSessionDisposition::Cancelled,
            TurnDispositionStorageKind::ReconciliationRequired => {
                ProgramSessionDisposition::Ambiguous
            }
            TurnDispositionStorageKind::Retired => ProgramSessionDisposition::Retired,
        };
        let frontier = row.try_get::<Option<Uuid>, _>("terminal_frontier_id")?;
        if frontier.is_none() != (kind == TurnDispositionStorageKind::Retired) {
            return Err(ProgramSessionError::Corruption("terminal frontier"));
        }
        let mut evidence = Vec::new();
        evidence.extend_from_slice(session.into_uuid().as_bytes());
        evidence.extend_from_slice(turn.into_uuid().as_bytes());
        evidence.extend_from_slice(accepted_input.into_uuid().as_bytes());
        if let Some(frontier) = frontier {
            evidence.extend_from_slice(frontier.as_bytes());
        }
        evidence.extend_from_slice(turn_disposition_kind_to_str(kind).as_bytes());
        Ok(Some(ProgramSessionOutcome {
            session,
            turn,
            accepted_input,
            disposition,
            digest: ProgramContentDigest::of(&evidence),
        }))
    }
}

// The caller supplies a run and the registration key joined from its stored reference.
pub(crate) async fn recorded_capability(
    run: Option<Uuid>,
    verified: Option<Uuid>,
) -> Result<Option<signalbox_domain::program_session::ProgramSessionCapability>, ()> {
    let run = match (run, verified) {
        (None, None) => return Ok(None),
        (Some(run), Some(verified)) if run == verified => ProgramRunId::from_uuid(run),
        _ => return Err(()),
    };
    struct StoredRun(ProgramRunId);
    impl signalbox_domain::program_session::ProgramRunVerifier for StoredRun {
        type Error = std::convert::Infallible;
        async fn verify_run(&self, candidate: ProgramRunId) -> Result<bool, Self::Error> {
            Ok(self.0 == candidate)
        }
    }
    match ProgramSessionHost::new(StoredRun(run))
        .session_capability(run)
        .await
    {
        Ok(capability) => Ok(capability),
        Err(never) => match never {},
    }
}

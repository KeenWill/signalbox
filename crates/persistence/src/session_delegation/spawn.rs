//! Atomic delegated child creation and exact tool-request replay.

use super::*;
use signalbox_domain::{SemanticTranscriptEntryId, SessionDelegation};

/// Fresh identities used only when the exact spawning request has no result.
#[derive(Clone, Copy, Debug)]
pub struct SpawnSessionCandidates {
    pub child: SessionId,
    pub turn: TurnId,
    pub entry: SemanticTranscriptEntryId,
}

/// The immutable relationship or the reason execution was not authorized.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecordDelegationSpawnOutcome {
    Recorded(Box<SessionDelegation>),
    Rejected(DelegationOperationRejection),
}

impl SessionDelegationRepository {
    pub async fn record_spawn(
        &self,
        request: DelegatedSpawnRequest,
        dispatch: &ToolDispatchAuthority,
        candidates: SpawnSessionCandidates,
    ) -> Result<RecordDelegationSpawnOutcome, SessionDelegationRepositoryError> {
        self.record_spawn_with_source(request, DispatchSource::Issued(dispatch), candidates)
            .await
    }

    async fn record_spawn_with_source(
        &self,
        request: DelegatedSpawnRequest,
        source: DispatchSource<'_>,
        candidates: SpawnSessionCandidates,
    ) -> Result<RecordDelegationSpawnOutcome, SessionDelegationRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        let result = async {
            let parent = request.request().session();
            lock_delivery_session(&mut transaction, parent).await?;
            lock_tool_session(&mut transaction, parent).await?;
            if !source.matches_request(request.request()) {
                return Ok(RecordDelegationSpawnOutcome::Rejected(
                    DelegationOperationRejection::StaleDispatch {
                        state: DelegationRequestExecutionState::AttemptEnded,
                    },
                ));
            }
            let inventory = sqlx::query_scalar::<_, Uuid>(
                "SELECT spawning_tool_request_id FROM session_delegation
                  WHERE parent_session_id = $1 ORDER BY spawning_tool_request_id",
            )
            .bind(parent.into_uuid())
            .fetch_all(&mut *transaction)
            .await?;
            let mut replay = None;
            for id in inventory {
                let relation =
                    load_relation(&mut transaction, ToolRequestId::from_uuid(id)).await?;
                if relation.spawning_request() == request.request().id() {
                    replay = Some(relation);
                }
            }
            if let Some(relation) = replay {
                if relation.task() != request.task() || relation.policy() != request.policy() {
                    return Err(
                        SessionDelegationCorruption::Inconsistent("spawn request purpose").into(),
                    );
                }
                validate_replay_attempt(
                    &mut transaction,
                    request.request(),
                    source,
                    ToolEffectClass::ExternalEffect,
                    ToolAttemptEnd::Completed {
                        result: ToolResultContent::Text(spawn_receipt(&relation)?),
                    },
                    "stored spawn attempt",
                )
                .await?;
                return Ok(RecordDelegationSpawnOutcome::Recorded(Box::new(relation)));
            }
            let dispatch =
                match resolve_dispatch(&mut transaction, request.request(), source).await? {
                    ResolvedDelegationDispatch::Executable(dispatch) => *dispatch,
                    ResolvedDelegationDispatch::NonExecutable(state) => {
                        return Ok(RecordDelegationSpawnOutcome::Rejected(
                            DelegationOperationRejection::StaleDispatch { state },
                        ));
                    }
                };
            if dispatch.attempt().effect_class() != ToolEffectClass::ExternalEffect {
                return Err(SessionDelegationRepositoryError::InvalidTransition(
                    "spawn_session requires an external-effect attempt",
                ));
            }
            if session_exists(&mut transaction, candidates.child).await? {
                return Err(SessionDelegationRepositoryError::InvalidTransition(
                    "spawn child identity collision",
                ));
            }
            crate::session_placement::create_delegated_child(
                &mut transaction,
                &request,
                candidates,
            )
            .await?;
            let relation = load_relation(&mut transaction, request.request().id()).await?;
            let ended = complete_attempt(&dispatch, spawn_receipt(&relation)?)?;
            persist_ended_attempt(&mut transaction, &ended).await?;
            Ok(RecordDelegationSpawnOutcome::Recorded(Box::new(relation)))
        }
        .await;
        finish(transaction, result).await
    }

    pub async fn record_process_spawn(
        &self,
        session: SessionId,
        turn: TurnId,
        request: ToolRequestId,
        task: String,
        policy: ChildRelationshipPolicy,
        candidates: SpawnSessionCandidates,
    ) -> Result<
        ProcessDelegationOutcome<(DelegatedSpawnRequest, Box<SessionDelegation>)>,
        SessionDelegationRepositoryError,
    > {
        let mut connection = self.pool.acquire().await?;
        if !session_exists(&mut connection, session).await? {
            return Ok(ProcessDelegationOutcome::Rejected(
                ProcessDelegationRequestRejection::SessionNotFound,
            ));
        }
        let Some(stored) = load_request_by_id(&mut connection, request).await? else {
            return Ok(ProcessDelegationOutcome::Rejected(
                ProcessDelegationRequestRejection::ToolRequestNotFound,
            ));
        };
        if stored.session() != session {
            return Ok(ProcessDelegationOutcome::Rejected(
                ProcessDelegationRequestRejection::ToolRequestNotInSession,
            ));
        }
        if stored.turn() != turn {
            return Ok(ProcessDelegationOutcome::Rejected(
                ProcessDelegationRequestRejection::RequestNotInTurn,
            ));
        }
        let Ok(logical) = DelegatedSpawnRequest::parse(stored, task, policy) else {
            return Ok(ProcessDelegationOutcome::InvalidRequest);
        };
        drop(connection);
        Ok(
            match self
                .record_spawn_with_source(logical.clone(), DispatchSource::Reconstitute, candidates)
                .await?
            {
                RecordDelegationSpawnOutcome::Recorded(relation) => {
                    ProcessDelegationOutcome::Applied((logical, relation))
                }
                RecordDelegationSpawnOutcome::Rejected(reason) => {
                    ProcessDelegationOutcome::Rejected(
                        ProcessDelegationRequestRejection::Operation(reason),
                    )
                }
            },
        )
    }
}

fn spawn_receipt(
    relation: &SessionDelegation,
) -> Result<ToolResultText, SessionDelegationRepositoryError> {
    let relationship = match relation.policy() {
        ChildRelationshipPolicy::Background => serde_json::json!({"kind": "background"}),
        ChildRelationshipPolicy::Bound {
            on_parent_stopped,
            on_parent_cancelled,
        } => serde_json::json!({
            "kind": "bound",
            "on_parent_stopped": child_action(on_parent_stopped),
            "on_parent_cancelled": child_action(on_parent_cancelled),
        }),
    };
    ToolResultText::try_new(
        serde_json::json!({
            "result": "session_spawned",
            "tool_request_id": relation.spawning_request().as_uuid().to_string(),
            "child_session_id": relation.child().as_uuid().to_string(),
            "relationship": relationship,
        })
        .to_string(),
    )
    .map_err(|_| SessionDelegationRepositoryError::InvalidTransition("spawn receipt bounds"))
}

const fn child_action(action: BoundChildAction) -> &'static str {
    match action {
        BoundChildAction::KeepRunning => "keep_running",
        BoundChildAction::Stop => "stop",
        BoundChildAction::Cancel => "cancel",
    }
}

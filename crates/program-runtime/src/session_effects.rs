//! Session operations composed entirely through host-side repositories.

use crate::{
    LiveDeliveryFailure,
    effects::{EffectExecutor, EffectInvocation, EffectRecovery},
};
use deno_core::serde_json;
use serde::Deserialize;
use signalbox_domain::{
    DirectModelSelection, DurableCommandId, EffectRequest, FrozenAliasDefinition,
    InlineFramePayload, ModelAlias, ModelSelectionOverride, ModelSelectionRequest,
    PerInputConfigurationChoices, ProgramCapability, SessionConfigurationDefaults,
    SessionConfigurationDefaultsVersion, SessionId, UserContent,
    program_session::{
        ProgramSessionCreate, ProgramSessionDisposition, ProgramSessionOutcome, ProgramSessionTurn,
    },
};
use signalbox_persistence::program_session::{ProgramSessionError, ProgramSessionRepository};
use std::{future::Future, pin::Pin};

/// Adds session creation and turn-by-turn driving to a host's other effects.
pub struct SessionEffects<E, N, A> {
    sessions: ProgramSessionRepository,
    other: E,
    nudge: N,
    aliases: A,
}

impl<E, N, A> SessionEffects<E, N, A> {
    pub const fn new(sessions: ProgramSessionRepository, other: E, nudge: N, aliases: A) -> Self {
        Self {
            sessions,
            other,
            nudge,
            aliases,
        }
    }
}

impl<E: EffectExecutor, N: Fn(SessionId), A: Fn(ModelAlias) -> Option<FrozenAliasDefinition>>
    EffectExecutor for SessionEffects<E, N, A>
{
    fn recovery(&self, request: &EffectRequest) -> EffectRecovery {
        if request.capability() == ProgramCapability::Session {
            match request.method() {
                "turn" | "create" => EffectRecovery::Idempotent,
                _ => EffectRecovery::Ambiguous,
            }
        } else {
            self.other.recovery(request)
        }
    }

    fn adopt<'a>(
        &'a mut self,
        invocation: EffectInvocation<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<Option<InlineFramePayload>, LiveDeliveryFailure>> + 'a>>
    {
        Box::pin(async move {
            if invocation.request.capability() != ProgramCapability::Session {
                return self.other.adopt(invocation).await;
            }
            let result = match decode(invocation.request)? {
                SessionOperation::Unsupported => return Ok(Some(refused())),
                SessionOperation::Turn(input) => self
                    .sessions
                    .adopt_turn(invocation.run, input, &self.nudge)
                    .await
                    .map(|outcome| outcome.map(encode_outcome)),
                SessionOperation::Create(input) => self
                    .sessions
                    .adopt_creation(invocation.run, input)
                    .await
                    .map(|session| session.map(encode_creation)),
            };
            match result {
                Ok(outcome) => outcome.transpose(),
                Err(
                    ProgramSessionError::Refused
                    | ProgramSessionError::Conflict
                    | ProgramSessionError::InvalidCommand
                    | ProgramSessionError::Admission(_),
                ) => Ok(Some(refused())),
                Err(error) => Err(failure(error)),
            }
        })
    }

    fn execute<'a>(
        &'a mut self,
        invocation: EffectInvocation<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<InlineFramePayload, LiveDeliveryFailure>> + 'a>> {
        Box::pin(async move {
            if invocation.request.capability() != ProgramCapability::Session {
                return self.other.execute(invocation).await;
            }
            let result = match decode(invocation.request)? {
                SessionOperation::Unsupported => return Ok(refused()),
                SessionOperation::Turn(input) => self
                    .sessions
                    .drive_turn(invocation.run, input, &self.aliases, &self.nudge)
                    .await
                    .map(encode_outcome),
                SessionOperation::Create(input) => self
                    .sessions
                    .create(invocation.run, input)
                    .await
                    .map(encode_creation),
            };
            match result {
                Ok(outcome) => outcome,
                Err(
                    ProgramSessionError::Refused
                    | ProgramSessionError::Conflict
                    | ProgramSessionError::InvalidCommand
                    | ProgramSessionError::Admission(_),
                ) => Ok(refused()),
                Err(error) => Err(failure(error)),
            }
        })
    }
}

fn failure(error: ProgramSessionError) -> LiveDeliveryFailure {
    LiveDeliveryFailure::new(error.to_string())
}
fn refused() -> InlineFramePayload {
    InlineFramePayload::new(b"{\"outcome\":\"refused\"}".as_slice())
}

enum SessionOperation {
    Unsupported,
    Turn(ProgramSessionTurn),
    Create(ProgramSessionCreate),
}

#[derive(Deserialize)]
struct SessionTurnInput {
    command: String,
    session: String,
    text: String,
    defaults_version: u64,
}
#[derive(Deserialize)]
struct SessionCreateInput {
    command: String,
    model: String,
}

fn command_id(value: &str) -> Result<DurableCommandId, LiveDeliveryFailure> {
    value
        .parse()
        .map(DurableCommandId::from_uuid)
        .map_err(|_| LiveDeliveryFailure::new("invalid session command identity"))
}

fn decode(request: &EffectRequest) -> Result<SessionOperation, LiveDeliveryFailure> {
    match request.method() {
        "turn" => {
            let input: SessionTurnInput = serde_json::from_slice(request.payload().as_bytes())
                .map_err(|_| LiveDeliveryFailure::new("invalid session input"))?;
            let command = command_id(&input.command)?;
            let session = input
                .session
                .parse()
                .map(SessionId::from_uuid)
                .map_err(|_| LiveDeliveryFailure::new("invalid session identity"))?;
            let version = SessionConfigurationDefaultsVersion::try_from_u64(input.defaults_version)
                .ok_or_else(|| LiveDeliveryFailure::new("invalid session defaults version"))?;
            let content = UserContent::try_text(input.text)
                .map_err(|_| LiveDeliveryFailure::new("invalid session content"))?;
            Ok(SessionOperation::Turn(ProgramSessionTurn {
                command,
                session,
                content,
                configuration: PerInputConfigurationChoices::new(
                    version,
                    ModelSelectionOverride::UseSessionDefault,
                ),
            }))
        }
        "create" => {
            let input: SessionCreateInput = serde_json::from_slice(request.payload().as_bytes())
                .map_err(|_| LiveDeliveryFailure::new("invalid session creation"))?;
            let model = input
                .model
                .parse()
                .map(DirectModelSelection::from_uuid)
                .map_err(|_| LiveDeliveryFailure::new("invalid session model selection"))?;
            Ok(SessionOperation::Create(ProgramSessionCreate {
                command: command_id(&input.command)?,
                defaults: SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(model)),
            }))
        }
        _ => Ok(SessionOperation::Unsupported),
    }
}

fn encode_creation(session: SessionId) -> Result<InlineFramePayload, LiveDeliveryFailure> {
    encode(serde_json::json!({"session": session.into_uuid().to_string()}))
}

fn encode_outcome(
    outcome: ProgramSessionOutcome,
) -> Result<InlineFramePayload, LiveDeliveryFailure> {
    let disposition = match outcome.disposition {
        ProgramSessionDisposition::Completed => "completed",
        ProgramSessionDisposition::Refused => "refused",
        ProgramSessionDisposition::Failed => "failed",
        ProgramSessionDisposition::Cancelled => "cancelled",
        ProgramSessionDisposition::Retired => "retired",
        ProgramSessionDisposition::Ambiguous => "ambiguous",
    };
    encode(serde_json::json!({
        "session": outcome.session.into_uuid().to_string(),
        "turn": outcome.turn.into_uuid().to_string(),
        "accepted_input": outcome.accepted_input.into_uuid().to_string(),
        "digest": outcome.digest.as_bytes().as_slice(),
        "outcome": disposition,
    }))
}

fn encode(value: serde_json::Value) -> Result<InlineFramePayload, LiveDeliveryFailure> {
    serde_json::to_vec(&value)
        .map(InlineFramePayload::new)
        .map_err(|_| LiveDeliveryFailure::new("session answer encoding failed"))
}

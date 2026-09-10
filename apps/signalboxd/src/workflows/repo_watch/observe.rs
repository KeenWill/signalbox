//! Finite repository observation program and checked provider-effect boundary.
//! Governed by docs/spec/workflows.md and docs/spec/repo-watch.md.

use super::effects::RepoWatchEffectFailure;
use serde::{Deserialize, Serialize};
use signalbox_domain::{EffectRequest, InlineFramePayload, ProgramCapability, RepositorySlug};
use signalbox_module_repo_watch_v2::{
    EventProducer, RepoWatchStore,
    observation_workflow::{ObservationInvocation, ObservationOutcome, ObservationResult},
};
use signalbox_persistence::program_journal::ProgramJournalRepository;
use signalbox_workflow_runtime::{
    LiveDeliveryFailure,
    native::{NativeProgram, NativeProgramError, NativeValue, WorkflowContext},
};
use uuid::Uuid;

pub const OBSERVE_ENTRY: &str = "ObserveRepository";
pub const OBSERVE_REVISION: &str = "1";
pub const OBSERVE_METHOD: &str = "repo.observe";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Payload {
    method: String,
    effect: String,
    repository: String,
    producer: Producer,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Producer {
    Poll,
    Webhook,
}

/// Checked immutable input preserves the exact method payload for successor adoption.
#[derive(Clone, Debug)]
pub struct ObserveInput {
    effect: Uuid,
    repository: RepositorySlug,
    producer: EventProducer,
    bytes: Vec<u8>,
}

impl ObserveInput {
    pub const fn effect(&self) -> Uuid {
        self.effect
    }
    pub const fn repository(&self) -> &RepositorySlug {
        &self.repository
    }
    pub const fn producer(&self) -> EventProducer {
        self.producer
    }
    pub fn new(
        repository: RepositorySlug,
        producer: EventProducer,
    ) -> Result<Self, NativeProgramError> {
        let payload = Payload {
            method: OBSERVE_METHOD.into(),
            effect: Uuid::now_v7().to_string(),
            repository: repository.as_str().into(),
            producer: match producer {
                EventProducer::Poll => Producer::Poll,
                EventProducer::Webhook => Producer::Webhook,
            },
        };
        Self::decode(&serde_json::to_vec(&payload).map_err(native_error)?)
    }

    pub fn request(&self) -> EffectRequest {
        EffectRequest::new(
            ProgramCapability::RepoWatch,
            OBSERVE_METHOD.into(),
            InlineFramePayload::new(self.bytes.clone()),
        )
    }

    pub fn from_request(request: &EffectRequest) -> Option<Self> {
        (request.capability() == ProgramCapability::RepoWatch && request.method() == OBSERVE_METHOD)
            .then(|| Self::decode(request.payload().as_bytes()).ok())
            .flatten()
    }
}

impl NativeValue for ObserveInput {
    fn decode(bytes: &[u8]) -> Result<Self, NativeProgramError> {
        let payload: Payload = serde_json::from_slice(bytes).map_err(native_error)?;
        if payload.method != OBSERVE_METHOD {
            return Err(NativeProgramError::new(
                "invalid repository observation method",
            ));
        }
        Ok(Self {
            effect: Uuid::parse_str(&payload.effect).map_err(native_error)?,
            repository: RepositorySlug::try_new(payload.repository).map_err(native_error)?,
            producer: match payload.producer {
                Producer::Poll => EventProducer::Poll,
                Producer::Webhook => EventProducer::Webhook,
            },
            bytes: bytes.to_vec(),
        })
    }
    fn encode(&self) -> Result<Vec<u8>, NativeProgramError> {
        Ok(self.bytes.clone())
    }
}

/// A committed observation range, or explicit lack of recovery evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ObserveAnswer {
    Observed(ObservationResult),
    Ambiguous,
}

impl NativeValue for ObserveAnswer {
    fn decode(bytes: &[u8]) -> Result<Self, NativeProgramError> {
        if bytes == br#"{"outcome":"ambiguous"}"# {
            return Ok(Self::Ambiguous);
        }
        ObservationResult::decode(bytes)
            .map(Self::Observed)
            .ok_or_else(|| NativeProgramError::new("invalid repository observation answer"))
    }
    fn encode(&self) -> Result<Vec<u8>, NativeProgramError> {
        Ok(match self {
            Self::Observed(result) => result.encode(),
            Self::Ambiguous => br#"{"outcome":"ambiguous"}"#.to_vec(),
        })
    }
}

/// Requests one bounded poll or webhook unit, retaining its accepted range as the run result.
pub struct ObserveRepository;
impl NativeProgram for ObserveRepository {
    type Input = ObserveInput;
    type Output = ObserveAnswer;
    async fn run(
        mut context: WorkflowContext,
        input: ObserveInput,
    ) -> Result<ObserveAnswer, NativeProgramError> {
        let answer = context.effect(input.request()).await?;
        ObserveAnswer::decode(answer.as_bytes())
    }
}

/// Daemon-owned provider I/O, supplied with only the module's receipt-bound store.
pub trait RepositoryObserver {
    fn repository(&self) -> &RepositorySlug;
    fn observe(
        &mut self,
        store: RepoWatchStore,
        producer: EventProducer,
    ) -> impl std::future::Future<Output = Result<ObservationOutcome, LiveDeliveryFailure>>;
}

pub async fn adopt(
    store: &RepoWatchStore,
    input: &ObserveInput,
) -> Result<Option<InlineFramePayload>, LiveDeliveryFailure> {
    adopt_checked(store, input).await.map_err(Into::into)
}

pub(crate) async fn adopt_checked(
    store: &RepoWatchStore,
    input: &ObserveInput,
) -> Result<Option<InlineFramePayload>, RepoWatchEffectFailure> {
    let _lease = store.lock_observation(&input.repository).await?;
    adopt_locked(store, input).await
}

async fn adopt_locked(
    store: &RepoWatchStore,
    input: &ObserveInput,
) -> Result<Option<InlineFramePayload>, RepoWatchEffectFailure> {
    let receipt = store
        .observation_receipt_by_effect(input.effect)
        .await
        .map_err(failure)?;
    let receipt = match receipt {
        Some(receipt) => Some(receipt),
        None => store
            .observation_receipt(&input.repository)
            .await
            .map_err(failure)?,
    };
    let Some(receipt) = receipt else {
        return Ok(None);
    };
    if receipt.effect != input.effect || receipt.input != input.bytes {
        return Err(RepoWatchEffectFailure::Rejected(LiveDeliveryFailure::new(
            "repository observation receipt conflicts or awaits adoption",
        )));
    }
    Ok(Some(InlineFramePayload::new(receipt.result)))
}

pub async fn execute(
    store: &RepoWatchStore,
    input: &ObserveInput,
    observer: &mut impl RepositoryObserver,
) -> Result<InlineFramePayload, LiveDeliveryFailure> {
    execute_checked(store, input, observer)
        .await
        .map_err(Into::into)
}

pub(crate) async fn execute_checked(
    store: &RepoWatchStore,
    input: &ObserveInput,
    observer: &mut impl RepositoryObserver,
) -> Result<InlineFramePayload, RepoWatchEffectFailure> {
    let _lease = store
        .lock_observation(&input.repository)
        .await
        .map_err(failure)?;
    if let Some(result) = adopt_locked(store, input).await? {
        return Ok(result);
    }
    if observer.repository() != &input.repository {
        return Err(RepoWatchEffectFailure::Rejected(LiveDeliveryFailure::new(
            "repository observation is unavailable",
        )));
    }
    let store = store.with_observation_invocation(ObservationInvocation {
        effect: input.effect,
        input: input.bytes.clone(),
        repository: input.repository.clone(),
    });
    let outcome = match observer.observe(store.clone(), input.producer).await {
        Ok(outcome) => outcome,
        Err(error) => {
            tracing::warn!(repository = input.repository.as_str(), %error, "workflow repository observation failed");
            ObservationOutcome::Failed
        }
    };
    let result = store.finish_observation(outcome).await.map_err(failure)?;
    Ok(InlineFramePayload::new(result.encode()))
}

/// A receipt is releasable only after its exact request and result have reached a journal.
pub async fn acknowledge(
    store: &RepoWatchStore,
    journals: &ProgramJournalRepository,
) -> Result<(), LiveDeliveryFailure> {
    for receipt in store.observation_receipts().await.map_err(failure)? {
        let input = ObserveInput::decode(&receipt.input).map_err(failure)?;
        if input.effect != receipt.effect {
            return Err(LiveDeliveryFailure::new(
                "invalid observation receipt identity",
            ));
        }
        if journals
            .has_effect_answer(
                &input.request(),
                &InlineFramePayload::new(receipt.result.clone()),
            )
            .await
            .map_err(failure)?
        {
            store.release_observation(&receipt).await.map_err(failure)?;
        }
    }
    Ok(())
}

fn native_error(error: impl std::fmt::Display) -> NativeProgramError {
    NativeProgramError::new(error.to_string())
}
pub(crate) fn failure(error: impl std::fmt::Display) -> LiveDeliveryFailure {
    LiveDeliveryFailure::new(error.to_string())
}

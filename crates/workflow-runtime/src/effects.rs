//! Grant-checked host execution and explicit recovery of effect requests.

use std::{collections::BTreeSet, future::Future, pin::Pin};

use deno_core::serde_json;
use serde::Deserialize;
use signalbox_domain::{
    DeliveryKind, EffectRequest, InlineFramePayload, JournalFrame, ProgramCapability, ProgramFault,
    ProgramRegistrationId, ProgramRunId, RejectReason, RequestFrame, RequestKind, RequestOrdinal,
    program_registration::{ProgramExecutable, ProgramGrants, ProgramRegistrationRequest},
};
use signalbox_persistence::program_registration::{
    ProgramRegistrationError, ProgramRegistrationRepository,
};

use crate::{
    LiveDeliveryFailure, LiveDeliverySource, ProgramArtifact, ProgramExecutionOutcome,
    WorkflowHost, WorkflowHostError,
};

/// How an operation with no durable answer may be attempted after a crash.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EffectRecovery {
    Idempotent,
    Ambiguous,
}

/// One host-owned effect, identified by its run and durable request ordinal.
#[derive(Clone, Copy, Debug)]
pub struct EffectInvocation<'a> {
    pub run: ProgramRunId,
    pub ordinal: RequestOrdinal,
    pub request: &'a EffectRequest,
}

/// Host services own effect implementations and proof of completed operations.
pub trait EffectExecutor {
    fn recovery(&self, request: &EffectRequest) -> EffectRecovery;

    /// Returns an answer only when the operation's durable record proves it completed.
    fn adopt<'a>(
        &'a mut self,
        invocation: EffectInvocation<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<Option<InlineFramePayload>, LiveDeliveryFailure>> + 'a>>;

    fn execute<'a>(
        &'a mut self,
        invocation: EffectInvocation<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<InlineFramePayload, LiveDeliveryFailure>> + 'a>>;
}

impl WorkflowHost {
    /// Resolves the pinned executable and grants before starting either adapter.
    #[allow(
        clippy::result_large_err,
        reason = "The host retains its replay fault inline."
    )]
    pub async fn execute_registered(
        &self,
        run: ProgramRunId,
        primitives: &mut impl LiveDeliverySource,
        effects: &mut impl EffectExecutor,
    ) -> Result<ProgramExecutionOutcome, WorkflowHostError> {
        let journal = self
            .journal
            .load(run)
            .await?
            .ok_or(WorkflowHostError::JournalMissing(run))?;
        let registrations = self.journal.registrations();
        if let Some(outcome) = crate::journal_outcome(&journal) {
            registrations
                .input_for_run(run)
                .await?
                .ok_or(ProgramRegistrationError::RunMissing)?;
            return Ok(outcome);
        }
        let registration = registrations
            .for_run(run)
            .await?
            .ok_or(ProgramRegistrationError::RunMissing)?;
        let recovered = journal
            .entries()
            .iter()
            .filter_map(|entry| match entry.frame() {
                JournalFrame::Request(frame) => Some(frame.ordinal()),
                JournalFrame::Delivery(_) => None,
            })
            .collect();
        let mut deliveries = GrantedDeliveries {
            run,
            grants: registration.content.grants,
            recovered,
            registrations: &registrations,
            primitives,
            effects,
        };
        let result = match registration.content.executable {
            ProgramExecutable::JavaScript { artifact, .. } => {
                let input = registrations
                    .input_for_run(run)
                    .await?
                    .ok_or(ProgramRegistrationError::RunMissing)?;
                self.execute_loaded(
                    run,
                    journal,
                    &ProgramArtifact::new(artifact),
                    input.as_bytes(),
                    &mut deliveries,
                )
                .await
            }
            executable @ ProgramExecutable::Native { .. } => {
                match self
                    .native_catalog
                    .as_ref()
                    .and_then(|catalog| catalog.resolve(&executable))
                {
                    Some(entry) => {
                        let input = registrations
                            .input_for_run(run)
                            .await?
                            .ok_or(ProgramRegistrationError::RunMissing)?;
                        self.execute_native_loaded(
                            run,
                            journal,
                            entry,
                            input.as_bytes(),
                            &mut deliveries,
                        )
                        .await
                    }
                    None => {
                        let tail = journal
                            .entries()
                            .last()
                            .map_or(0, |entry| entry.position().as_u64());
                        self.native_fault(
                            run,
                            tail,
                            signalbox_domain::ProgramFault::ContractRetired(
                                InlineFramePayload::new(
                                    b"pinned native executable is unavailable".as_slice(),
                                ),
                            ),
                        )
                        .await
                    }
                }
            }
        };
        if let Some(outcome) = self
            .journal
            .load(run)
            .await?
            .and_then(|journal| crate::journal_outcome(&journal))
        {
            return Ok(outcome);
        }
        result
    }
}

fn registration_failure(error: ProgramRegistrationError) -> LiveDeliveryFailure {
    LiveDeliveryFailure::new(error.to_string())
}

struct GrantedDeliveries<'a, P, E> {
    run: ProgramRunId,
    grants: ProgramGrants,
    recovered: BTreeSet<RequestOrdinal>,
    registrations: &'a ProgramRegistrationRepository,
    primitives: &'a mut P,
    effects: &'a mut E,
}

impl<P: LiveDeliverySource, E: EffectExecutor> LiveDeliverySource for GrantedDeliveries<'_, P, E> {
    fn suspend_on_wait(&self, outstanding: &[RequestFrame]) -> bool {
        outstanding.iter().all(|frame| {
            request_capability(frame.kind())
                .is_none_or(|capability| self.grants.contains(capability))
        }) && self.primitives.suspend_on_wait(outstanding)
    }

    fn next_delivery<'a>(
        &'a mut self,
        outstanding: &'a [RequestFrame],
    ) -> Pin<Box<dyn Future<Output = Result<DeliveryKind, LiveDeliveryFailure>> + 'a>> {
        Box::pin(async move {
            for frame in outstanding {
                if let Some(capability) = request_capability(frame.kind())
                    && !self.grants.contains(capability)
                {
                    return Ok(DeliveryKind::Reject {
                        resolves: frame.ordinal(),
                        reason: RejectReason::CapabilityDenied,
                    });
                }
            }
            if let Some((frame, request)) =
                outstanding.iter().find_map(|frame| match frame.kind() {
                    RequestKind::Effect(request) => Some((frame, request)),
                    _ => None,
                })
            {
                let recovered = self.recovered.remove(&frame.ordinal());
                if request.capability() == ProgramCapability::Register {
                    let attempt = if recovered {
                        EffectAttempt::Recovered
                    } else {
                        EffectAttempt::Live
                    };
                    return self.register(frame, request, attempt).await;
                }
                let invocation = EffectInvocation {
                    run: self.run,
                    ordinal: frame.ordinal(),
                    request,
                };
                let payload = if recovered {
                    match self.effects.adopt(invocation).await? {
                        Some(outcome) => outcome,
                        None if self.effects.recovery(request) == EffectRecovery::Idempotent => {
                            self.effects.execute(invocation).await?
                        }
                        None => ambiguous_answer(),
                    }
                } else {
                    self.effects.execute(invocation).await?
                };
                return Ok(DeliveryKind::Answer {
                    resolves: frame.ordinal(),
                    payload,
                });
            }
            if let Some(frame) = outstanding
                .iter()
                .find(|frame| matches!(frame.kind(), RequestKind::Terminal(_)))
            {
                return Ok(if outstanding.len() == 1 {
                    DeliveryKind::Answer {
                        resolves: frame.ordinal(),
                        payload: InlineFramePayload::default(),
                    }
                } else {
                    DeliveryKind::Reject {
                        resolves: frame.ordinal(),
                        reason: RejectReason::OutstandingRequests,
                    }
                });
            }
            self.primitives.next_delivery(outstanding).await
        })
    }
}

enum EffectAttempt {
    Live,
    Recovered,
}

impl<P, E> GrantedDeliveries<'_, P, E> {
    async fn register(
        &self,
        frame: &RequestFrame,
        request: &EffectRequest,
        attempt: EffectAttempt,
    ) -> Result<DeliveryKind, LiveDeliveryFailure> {
        if request.method() != "register" {
            return Ok(DeliveryKind::Reject {
                resolves: frame.ordinal(),
                reason: RejectReason::UnsupportedOperation,
            });
        }
        let Ok(input) = serde_json::from_slice::<RegisterInput>(request.payload().as_bytes())
        else {
            return Ok(DeliveryKind::Reject {
                resolves: frame.ordinal(),
                reason: RejectReason::UnsupportedOperation,
            });
        };
        let Ok(registration_id) = input.id.parse().map(ProgramRegistrationId::from_uuid) else {
            return Ok(DeliveryKind::Reject {
                resolves: frame.ordinal(),
                reason: RejectReason::UnsupportedOperation,
            });
        };
        let grants = ProgramGrants::new(input.grants.into_iter().map(Into::into));
        if !self.grants.permits_child(&grants) {
            return Ok(DeliveryKind::Reject {
                resolves: frame.ordinal(),
                reason: RejectReason::CapabilityDenied,
            });
        }
        let input = ProgramRegistrationRequest {
            name: input.name,
            revision: input.revision,
            source: input.source,
            artifact: input.artifact,
            grants,
        };
        let registration = match attempt {
            EffectAttempt::Live => self
                .registrations
                .register_child(self.run, registration_id, input)
                .await
                .map(Some),
            EffectAttempt::Recovered => {
                self.registrations
                    .find(registration_id, &input.into_content())
                    .await
            }
        };
        let registration = match registration {
            Ok(Some(registration)) => registration,
            Ok(None) => {
                return Ok(DeliveryKind::Answer {
                    resolves: frame.ordinal(),
                    payload: ambiguous_answer(),
                });
            }
            Err(error @ ProgramRegistrationError::RegistrationConflict { .. }) => {
                return Ok(DeliveryKind::Fault(ProgramFault::ProgramError(
                    InlineFramePayload::new(error.to_string().into_bytes()),
                )));
            }
            Err(error) => return Err(registration_failure(error)),
        };
        let payload = serde_json::to_vec(
            &serde_json::json!({"registration": registration.id.into_uuid().to_string()}),
        )
        .map_err(|_| LiveDeliveryFailure::new("registration answer encoding failed"))?;
        Ok(DeliveryKind::Answer {
            resolves: frame.ordinal(),
            payload: InlineFramePayload::new(payload),
        })
    }
}

fn ambiguous_answer() -> InlineFramePayload {
    InlineFramePayload::new(b"{\"outcome\":\"ambiguous\"}".as_slice())
}

pub(crate) fn request_capability(kind: &RequestKind) -> Option<ProgramCapability> {
    match kind {
        RequestKind::Now(_) => Some(ProgramCapability::Time),
        RequestKind::Random(_) => Some(ProgramCapability::Random),
        RequestKind::Sleep(_) => Some(ProgramCapability::Sleep),
        RequestKind::AwaitEvent(_) => Some(ProgramCapability::Subscribe),
        RequestKind::Effect(effect) => Some(effect.capability()),
        RequestKind::Scope(_) | RequestKind::Terminal(_) => None,
    }
}

#[derive(Deserialize)]
struct RegisterInput {
    id: String,
    name: String,
    revision: String,
    source: Vec<u8>,
    artifact: String,
    grants: Vec<IsolateCapability>,
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum IsolateCapability {
    Time,
    Random,
    Sleep,
    Subscribe,
    Session,
    Judge,
    ExecStage,
    Corpus,
    EvalRecord,
    Blob,
    Register,
    /// Checked repository-watch module operations.
    RepoWatch,
}

impl From<IsolateCapability> for ProgramCapability {
    fn from(value: IsolateCapability) -> Self {
        match value {
            IsolateCapability::Time => Self::Time,
            IsolateCapability::Random => Self::Random,
            IsolateCapability::Sleep => Self::Sleep,
            IsolateCapability::Subscribe => Self::Subscribe,
            IsolateCapability::Session => Self::Session,
            IsolateCapability::Judge => Self::Judge,
            IsolateCapability::ExecStage => Self::ExecStage,
            IsolateCapability::Corpus => Self::Corpus,
            IsolateCapability::EvalRecord => Self::EvalRecord,
            IsolateCapability::Blob => Self::Blob,
            IsolateCapability::Register => Self::Register,
            IsolateCapability::RepoWatch => Self::RepoWatch,
        }
    }
}

#[cfg(feature = "postgres-integration")]
pub(crate) struct NoEffects<'a, P>(pub &'a mut P);
#[cfg(feature = "postgres-integration")]
impl<P: LiveDeliverySource> LiveDeliverySource for NoEffects<'_, P> {
    fn next_delivery<'a>(
        &'a mut self,
        outstanding: &'a [RequestFrame],
    ) -> Pin<Box<dyn Future<Output = Result<DeliveryKind, LiveDeliveryFailure>> + 'a>> {
        Box::pin(async move {
            if let Some(frame) = outstanding
                .iter()
                .find(|frame| matches!(frame.kind(), RequestKind::Effect(_)))
            {
                return Ok(DeliveryKind::Reject {
                    resolves: frame.ordinal(),
                    reason: RejectReason::CapabilityDenied,
                });
            }
            self.0.next_delivery(outstanding).await
        })
    }
}

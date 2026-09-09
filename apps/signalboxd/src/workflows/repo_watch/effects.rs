//! Grant-checked repository-watch effects over the retained module store and command sink.
//! Governed by docs/spec/workflows.md and docs/spec/repo-watch.md.

use serde::{Deserialize, Serialize};
use signalbox_domain::{
    EffectRequest, InlineFramePayload, ProgramCapability, RepoWatchDispatchId, RepoWatchRuleId,
    RepositorySlug, SessionTemplateName,
};
use signalbox_module_repo_watch_v2::{
    CreateSessionCommandFactory, DispatchReferenceGenerator, RepoWatchStore, SessionCommandCodec,
    dispatch::SessionCommandSink,
    workflow::{EffectReceipt, EvaluationInvocation, RuleContext, SubmissionInvocation},
};
use signalbox_session_ownership::{LifecycleEventSource, OffsetDateTime};
use signalbox_workflow_runtime::{
    LiveDeliveryFailure,
    effects::{EffectExecutor, EffectInvocation, EffectRecovery},
};
use std::{future::Future, pin::Pin};
use uuid::Uuid;

/// Checked program requests. Mutation identities are independent of journal run identities.
#[derive(Clone, Debug)]
pub enum RepoWatchRequest {
    NextRuleEvent {
        repository: RepositorySlug,
        rule: RepoWatchRuleId,
    },
    CommitEvaluation {
        effect: Uuid,
        context: Box<RuleContext>,
        plan: Vec<SessionTemplateName>,
    },
    SubmitPending {
        effect: Uuid,
        dispatch: RepoWatchDispatchId,
    },
}

/// Decodes an idle read or a checked rule/event context, refusing malformed answers.
pub fn decode_rule_event(
    payload: &InlineFramePayload,
) -> Result<Option<RuleContext>, LiveDeliveryFailure> {
    if payload.as_bytes().is_empty() {
        Ok(None)
    } else {
        RuleContext::decode(payload.as_bytes())
            .map(Some)
            .ok_or_else(invalid)
    }
}

/// Submission completion or the host's explicit lack of recovery evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubmissionOutcome {
    Submitted,
    Ambiguous,
}

impl SubmissionOutcome {
    pub fn decode(payload: &InlineFramePayload) -> Option<Self> {
        match payload.as_bytes() {
            b"submitted" => Some(Self::Submitted),
            br#"{"outcome":"ambiguous"}"# => Some(Self::Ambiguous),
            _ => None,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "method", deny_unknown_fields)]
enum Payload {
    #[serde(rename = "repo.nextRuleEvent")]
    NextRuleEvent { repository: String, rule: String },
    #[serde(rename = "repo.commitEvaluation")]
    CommitEvaluation {
        effect: String,
        context: Vec<u8>,
        plan: Vec<String>,
    },
    #[serde(rename = "repo.submitPending")]
    SubmitPending { effect: String, dispatch: String },
}

impl RepoWatchRequest {
    pub fn encode(&self) -> Result<EffectRequest, LiveDeliveryFailure> {
        let (method, payload) = match self {
            Self::NextRuleEvent { repository, rule } => (
                "repo.nextRuleEvent",
                Payload::NextRuleEvent {
                    repository: repository.as_str().into(),
                    rule: rule.as_str().into(),
                },
            ),
            Self::CommitEvaluation {
                effect,
                context,
                plan,
            } => (
                "repo.commitEvaluation",
                Payload::CommitEvaluation {
                    effect: effect.to_string(),
                    context: context.encode().map_err(failure)?,
                    plan: plan.iter().map(|v| v.as_str().into()).collect(),
                },
            ),
            Self::SubmitPending { effect, dispatch } => (
                "repo.submitPending",
                Payload::SubmitPending {
                    effect: effect.to_string(),
                    dispatch: dispatch.into_uuid().to_string(),
                },
            ),
        };
        Ok(EffectRequest::new(
            ProgramCapability::RepoWatch,
            method.into(),
            InlineFramePayload::new(serde_json::to_vec(&payload).map_err(failure)?),
        ))
    }

    pub fn decode(request: &EffectRequest) -> Option<Self> {
        if request.capability() != ProgramCapability::RepoWatch {
            return None;
        }
        let raw: Payload = serde_json::from_slice(request.payload().as_bytes()).ok()?;
        Some(match (request.method(), raw) {
            ("repo.nextRuleEvent", Payload::NextRuleEvent { repository, rule }) => {
                Self::NextRuleEvent {
                    repository: RepositorySlug::try_new(repository).ok()?,
                    rule: RepoWatchRuleId::try_new(rule).ok()?,
                }
            }
            (
                "repo.commitEvaluation",
                Payload::CommitEvaluation {
                    effect,
                    context,
                    plan,
                },
            ) => Self::CommitEvaluation {
                effect: Uuid::parse_str(&effect).ok()?,
                context: Box::new(RuleContext::decode(&context)?),
                plan: plan
                    .into_iter()
                    .map(|v| SessionTemplateName::try_new(v).ok())
                    .collect::<Option<_>>()?,
            },
            ("repo.submitPending", Payload::SubmitPending { effect, dispatch }) => {
                Self::SubmitPending {
                    effect: Uuid::parse_str(&effect).ok()?,
                    dispatch: RepoWatchDispatchId::from_uuid(Uuid::parse_str(&dispatch).ok()?),
                }
            }
            _ => return None,
        })
    }
}

/// Daemon-owned I/O; programs receive only checked request and result values.
pub struct RepoWatchEffects<Ids, Factory, Codec, Sink> {
    pub store: RepoWatchStore,
    pub rules: std::collections::BTreeMap<RepositorySlug, Vec<signalbox_domain::RepoWatchRule>>,
    pub ids: Ids,
    pub factory: Factory,
    pub codec: Codec,
    /// The retained module command sink, including checkout and core receipt adoption.
    pub sink: Sink,
    pub source: LifecycleEventSource,
}

impl<Ids, Factory, Codec, Sink> RepoWatchEffects<Ids, Factory, Codec, Sink> {
    /// Releases a receipt after loading its exact request and answer from either run.
    pub async fn acknowledge_receipt(
        &self,
        journals: &signalbox_persistence::program_journal::ProgramJournalRepository,
        run: signalbox_domain::ProgramRunId,
        receipt: &EffectReceipt,
    ) -> Result<(), LiveDeliveryFailure> {
        use signalbox_domain::{DeliveryKind, JournalFrame, RequestKind};
        let journal = journals
            .load(run)
            .await
            .map_err(failure)?
            .ok_or_else(invalid)?;
        let delivered = journal.entries().iter().any(|entry| {
            let JournalFrame::Request(frame) = entry.frame() else { return false; };
            let RequestKind::Effect(request) = frame.kind() else { return false; };
            if request.capability() != ProgramCapability::RepoWatch || request.payload().as_bytes() != receipt.input { return false; }
            if !matches!(RepoWatchRequest::decode(request), Some(RepoWatchRequest::CommitEvaluation { effect, .. } | RepoWatchRequest::SubmitPending { effect, .. }) if effect == receipt.effect) { return false; }
            journal.entries().iter().any(|entry| matches!(entry.frame(), JournalFrame::Delivery(delivery) if matches!(delivery.kind(), DeliveryKind::Answer { resolves, payload } if *resolves == frame.ordinal() && payload.as_bytes() == receipt.result)))
        });
        if !delivered {
            return Err(LiveDeliveryFailure::new(
                "repository-watch receipt has no durable delivery",
            ));
        }
        self.store
            .release_evaluation(receipt)
            .await
            .map_err(failure)?;
        self.store
            .release_submission(receipt)
            .await
            .map_err(failure)
    }
}

impl<
    Ids: DispatchReferenceGenerator,
    Factory: CreateSessionCommandFactory,
    Codec: SessionCommandCodec,
    Sink: SessionCommandSink,
> EffectExecutor for RepoWatchEffects<Ids, Factory, Codec, Sink>
{
    fn recovery(&self, request: &EffectRequest) -> EffectRecovery {
        recovery(request)
    }
    fn adopt<'a>(
        &'a mut self,
        invocation: EffectInvocation<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<Option<InlineFramePayload>, LiveDeliveryFailure>> + 'a>>
    {
        Box::pin(async move {
            let request = RepoWatchRequest::decode(invocation.request).ok_or_else(invalid)?;
            let input = invocation.request.payload().as_bytes();
            match request {
                RepoWatchRequest::NextRuleEvent { .. } => Ok(None),
                RepoWatchRequest::CommitEvaluation { effect, .. } => self
                    .store
                    .adopt_evaluation(effect, input)
                    .await
                    .map(|v| v.map(InlineFramePayload::new))
                    .map_err(failure),
                RepoWatchRequest::SubmitPending { effect, .. } => match self
                    .store
                    .submission_receipt(effect, input)
                    .await
                    .map_err(failure)?
                {
                    Some(signalbox_module_repo_watch_v2::workflow::SubmissionReceipt {
                        result: Some(result),
                        ..
                    }) => Ok(Some(InlineFramePayload::new(result))),
                    Some(signalbox_module_repo_watch_v2::workflow::SubmissionReceipt {
                        result: None,
                        ..
                    }) => self.execute(invocation).await.map(Some),
                    None => Ok(None),
                },
            }
        })
    }
    fn execute<'a>(
        &'a mut self,
        invocation: EffectInvocation<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<InlineFramePayload, LiveDeliveryFailure>> + 'a>> {
        Box::pin(async move {
            let request = RepoWatchRequest::decode(invocation.request).ok_or_else(invalid)?;
            let input = invocation.request.payload().as_bytes();
            let result = match request {
                RepoWatchRequest::NextRuleEvent { repository, rule } => {
                    let Some(rule) = self
                        .rules
                        .get(&repository)
                        .and_then(|rules| rules.iter().find(|candidate| candidate.id() == &rule))
                    else {
                        return Ok(InlineFramePayload::default());
                    };
                    self.store
                        .next_rule_context(&repository, rule)
                        .await
                        .map_err(failure)?
                        .map(|v| v.encode())
                        .transpose()
                        .map_err(failure)?
                        .unwrap_or_default()
                }
                RepoWatchRequest::CommitEvaluation {
                    effect,
                    context,
                    plan,
                } => self
                    .store
                    .commit_evaluation(
                        EvaluationInvocation {
                            effect,
                            input,
                            context: &context,
                            plan: &plan,
                            now: OffsetDateTime::now_utc(),
                        },
                        &mut self.ids,
                        &mut self.factory,
                        &mut self.codec,
                    )
                    .await
                    .map_err(failure)?,
                RepoWatchRequest::SubmitPending { effect, dispatch } => self
                    .store
                    .submit_dispatch(
                        SubmissionInvocation {
                            effect,
                            input,
                            dispatch,
                        },
                        &mut self.codec,
                        &mut self.sink,
                        &self.source,
                    )
                    .await
                    .map_err(|_| LiveDeliveryFailure::new("repository-watch submission failed"))?,
            };
            Ok(InlineFramePayload::new(result))
        })
    }
}

pub(crate) fn recovery(request: &EffectRequest) -> EffectRecovery {
    match RepoWatchRequest::decode(request) {
        Some(
            RepoWatchRequest::NextRuleEvent { .. } | RepoWatchRequest::CommitEvaluation { .. },
        ) => EffectRecovery::Idempotent,
        _ => EffectRecovery::Ambiguous,
    }
}

fn invalid() -> LiveDeliveryFailure {
    LiveDeliveryFailure::new("invalid repository-watch effect")
}
fn failure(error: impl std::fmt::Display) -> LiveDeliveryFailure {
    LiveDeliveryFailure::new(error.to_string())
}

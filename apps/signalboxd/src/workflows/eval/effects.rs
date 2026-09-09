//! Catalog reads and production judge execution behind the workflow journal.

use super::*;
use crate::{
    approval_judge_eval::{
        ApprovalJudgeEvalBinding, ApprovalJudgeEvalCase, ApprovalJudgeEvalDispatchFence,
        judge_eval_case, render_eval_case,
    },
    blob_read_runtime::{BLOB_READ_TIMEOUT, BlobReadError, read_blob_chunk, read_blob_entry},
    blob_storage_runtime::BlobStoreRegistry,
    configuration::HubModelConfiguration,
};
use signalbox_domain::{
    BlobDigest, DirectModelSelection, InlineFramePayload, ProgramCapability, ProgramRunId,
    ProviderModelIdentity, ResolvedProviderTarget,
};
use signalbox_model_provider_runtime::{
    ApprovalJudgeModel, ApprovalJudgeModelError, ApprovalJudgeModelRequest,
    PreparedApprovalJudgeModelCall,
};
use signalbox_model_runtime::TokenUsage;
use signalbox_persistence::{
    blob::BlobCatalogRepository, program_journal::ProgramJournalRepository,
    program_registration::ProgramRegistrationRepository,
};
use signalbox_workflow_runtime::{
    LiveDeliveryFailure,
    effects::{EffectExecutor, EffectInvocation, EffectRecovery},
    native::NativeValue,
};
use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
};

/// Resolved host services; provider simulation is supplied only by tests.
#[derive(Clone)]
pub struct EvalServices {
    registrations: ProgramRegistrationRepository,
    journal: ProgramJournalRepository,
    blobs: Arc<dyn CorpusBlobs>,
    model: Arc<dyn ApprovalJudgeModel>,
    binding: JudgeBinding,
    configuration: Arc<HubModelConfiguration>,
}

impl EvalServices {
    pub fn new(
        pool: sqlx::PgPool,
        stores: Arc<BlobStoreRegistry>,
        model: Arc<dyn ApprovalJudgeModel>,
        binding: JudgeBinding,
        configuration: Arc<HubModelConfiguration>,
    ) -> Self {
        Self {
            registrations: ProgramRegistrationRepository::new(pool.clone()),
            journal: ProgramJournalRepository::new(pool.clone()),
            blobs: Arc::new(CatalogBlobs {
                repository: BlobCatalogRepository::new(pool),
                stores,
            }),
            model,
            binding,
            configuration,
        }
    }

    async fn manifest(&self, run: ProgramRunId) -> Result<EvalManifest, EvalFailure> {
        let bytes = self
            .registrations
            .input_for_run(run)
            .await
            .map_err(infrastructure_failure)?
            .ok_or_else(|| failure("evaluation run input missing"))?;
        EvalManifest::decode(bytes.as_bytes()).map_err(failure)
    }

    async fn corpus(&self, manifest: &EvalManifest) -> Result<CorpusAnswer, EvalFailure> {
        let digest = manifest.corpus.parse().map_err(failure)?;
        let bytes = self.blobs.read(digest).await?;
        if BlobDigest::digest(&bytes) != digest {
            return Err(failure("corpus digest mismatch"));
        }
        let cases: Vec<Case> = match manifest.format {
            CorpusFormat::Offline => signalbox_approval_judge_eval::decode_corpus(&bytes)
                .map_err(failure)?
                .cases
                .into_iter()
                .map(Case::Offline)
                .collect(),
            CorpusFormat::Live => std::str::from_utf8(&bytes)
                .map_err(failure)?
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(|line| serde_json::from_str(line).map(Case::Live))
                .collect::<Result<_, _>>()
                .map_err(failure)?,
        };
        let selected = manifest
            .cases
            .iter()
            .map(|position| {
                cases
                    .get(*position as usize)
                    .cloned()
                    .ok_or_else(|| failure("selected corpus case missing"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        // Preflight every selected case before the first provider operation.
        let rendered = selected
            .iter()
            .map(|case| render_eval_case(&eval_case(case)))
            .collect::<Result<Vec<_>, _>>()
            .map_err(failure)?;
        let rendered = rendered
            .into_iter()
            .map(|payload| payload + "\n")
            .collect::<String>();
        Ok(CorpusAnswer {
            cases: selected,
            corpus_digest: stable_digest(&bytes),
            rendered_digest: stable_digest(rendered.as_bytes()),
        })
    }

    fn validate_binding(&self, manifest: &EvalManifest) -> Result<(), EvalFailure> {
        if manifest.binding != self.binding {
            return Err(failure("pinned judge binding is unavailable"));
        }
        Ok(())
    }

    async fn judge(
        &self,
        manifest: &EvalManifest,
        trial: TrialRequest,
    ) -> Result<JudgeAnswer, EvalFailure> {
        self.validate_binding(manifest)?;
        let corpus = self.corpus(manifest).await?;
        let case = eval_case(&corpus.cases[(trial.trial / manifest.repeats) as usize]);
        let request_digest =
            BlobDigest::digest(render_eval_case(&case).map_err(failure)?.as_bytes()).to_string();
        let binding = ApprovalJudgeEvalBinding {
            selection: DirectModelSelection::from_uuid(
                uuid::Uuid::parse_str(&self.binding.selection).map_err(failure)?,
            ),
            target: ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(
                uuid::Uuid::parse_str(&self.binding.target).map_err(failure)?,
            )),
            credential_reference: self.binding.credential_reference.clone(),
        };
        let observer = ObservedJudge {
            model: self.model.clone(),
            call: Mutex::new(None),
        };
        let result = judge_eval_case(&observer, &binding, &case).await;
        let call = observer.call.into_inner().map_err(failure)?;
        let failed = |cause: String, usage, provider_reported_model| JudgeAnswer::Failed {
            call: call.clone(),
            request_digest: request_digest.clone(),
            binding: self.binding.clone(),
            cause,
            provider_reported_model,
            usage: usage_record(usage),
        };
        Ok(match result {
            Ok(verdict) => {
                if crate::usage_limits::approval_judge_usage_exceeds_configured_limits(
                    &self.configuration,
                    binding.target,
                    verdict.usage,
                ) != Some(false)
                {
                    failed(
                        "usage_limit_exceeded".into(),
                        verdict.usage,
                        verdict.provider_reported_model,
                    )
                } else {
                    JudgeAnswer::Verdict {
                        call: call.ok_or_else(|| failure("judge call identity missing"))?,
                        request_digest,
                        binding: self.binding.clone(),
                        actual: verdict.recommendation.into(),
                        rationale: verdict.rationale,
                        provider_reported_model: verdict.provider_reported_model,
                        usage: usage_record(verdict.usage),
                    }
                }
            }
            Err(error) => {
                let error = error
                    .downcast_ref::<ApprovalJudgeModelError>()
                    .ok_or_else(|| failure("judge case failed after preflight"))?;
                failed(
                    judge_failure(error).into(),
                    error.usage(),
                    error
                        .reported_model()
                        .map(|model| model.as_str().to_owned()),
                )
            }
        })
    }
}

/// An attempt's effect adapter; each answer is persisted by the common host.
pub struct EvaluationEffects {
    services: EvalServices,
    rejected: bool,
}
impl EvaluationEffects {
    pub fn new(services: EvalServices) -> Self {
        Self {
            services,
            rejected: false,
        }
    }

    pub(crate) fn rejected(&self) -> bool {
        self.rejected
    }

    fn finish<T>(&mut self, result: Result<T, EvalFailure>) -> Result<T, LiveDeliveryFailure> {
        self.rejected = matches!(&result, Err(EvalFailure::Rejected(_)));
        result.map_err(|error| match error {
            EvalFailure::Rejected(error) | EvalFailure::Infrastructure(error) => error,
        })
    }

    async fn judge_trial(
        &self,
        invocation: EffectInvocation<'_>,
    ) -> Result<(EvalManifest, TrialRequest), EvalFailure> {
        let trial: TrialRequest =
            decode(invocation.request.payload().as_bytes()).map_err(failure)?;
        let manifest = self.services.manifest(invocation.run).await?;
        if trial.trial >= manifest.trial_count().map_err(failure)? {
            return Err(failure("trial is outside manifest"));
        }
        let journal = self
            .services
            .journal
            .load(invocation.run)
            .await
            .map_err(infrastructure_failure)?
            .ok_or_else(|| failure("evaluation journal missing"))?;
        let preceding_trials = journal.entries().iter().filter(|entry| {
            matches!(entry.frame(), signalbox_domain::JournalFrame::Request(frame)
                if frame.ordinal() < invocation.ordinal && matches!(frame.kind(), signalbox_domain::RequestKind::Effect(effect)
                    if effect.capability() == ProgramCapability::Judge && effect.method() == "evaluate"))
        }).count();
        if preceding_trials != trial.trial as usize {
            return Err(failure(
                "judge request does not follow manifest trial order",
            ));
        }
        self.services.validate_binding(&manifest)?;
        Ok((manifest, trial))
    }

    async fn execute_inner(
        &self,
        invocation: EffectInvocation<'_>,
    ) -> Result<InlineFramePayload, EvalFailure> {
        let bytes = invocation.request.payload().as_bytes();
        let answer = match (invocation.request.capability(), invocation.request.method()) {
            (ProgramCapability::Corpus, "load") => {
                let _: Empty = decode(bytes).map_err(failure)?;
                let manifest = self.services.manifest(invocation.run).await?;
                encode(&self.services.corpus(&manifest).await?).map_err(failure)?
            }
            (ProgramCapability::Judge, "evaluate") => {
                let (manifest, trial) = self.judge_trial(invocation).await?;
                encode(&self.services.judge(&manifest, trial).await?).map_err(failure)?
            }
            (ProgramCapability::Blob, "read") => {
                let input: BlobReadRequest = decode(bytes).map_err(failure)?;
                let digest = input.digest.parse().map_err(failure)?;
                let bytes = self.services.blobs.read(digest).await?;
                if BlobDigest::digest(&bytes) != digest {
                    return Err(failure("blob digest mismatch"));
                }
                encode(&BlobAnswer { bytes }).map_err(failure)?
            }
            _ => return Err(failure("unsupported evaluation operation")),
        };
        Ok(InlineFramePayload::new(answer))
    }
}

impl EffectExecutor for EvaluationEffects {
    fn recovery(&self, request: &signalbox_domain::EffectRequest) -> EffectRecovery {
        if request.capability() == ProgramCapability::Judge
            && request.method() == "evaluate"
            && decode::<TrialRequest>(request.payload().as_bytes()).is_ok()
        {
            EffectRecovery::Ambiguous
        } else {
            EffectRecovery::Idempotent
        }
    }
    fn adopt<'a>(
        &'a mut self,
        invocation: EffectInvocation<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<Option<InlineFramePayload>, LiveDeliveryFailure>> + 'a>>
    {
        Box::pin(async move {
            let result = if invocation.request.capability() == ProgramCapability::Judge
                && invocation.request.method() == "evaluate"
            {
                self.judge_trial(invocation).await.map(|_| None)
            } else {
                Ok(None)
            };
            self.finish(result)
        })
    }
    fn execute<'a>(
        &'a mut self,
        invocation: EffectInvocation<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<InlineFramePayload, LiveDeliveryFailure>> + 'a>> {
        Box::pin(async move {
            let result = self.execute_inner(invocation).await;
            self.finish(result)
        })
    }
}

trait CorpusBlobs: Send + Sync {
    fn read(
        &self,
        digest: BlobDigest,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, EvalFailure>> + Send + '_>>;
}
struct CatalogBlobs {
    repository: BlobCatalogRepository,
    stores: Arc<BlobStoreRegistry>,
}
impl CorpusBlobs for CatalogBlobs {
    fn read(
        &self,
        digest: BlobDigest,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, EvalFailure>> + Send + '_>> {
        Box::pin(async move {
            let _permit = self
                .stores
                .read_budget()
                .try_acquire_owned()
                .map_err(infrastructure_failure)?;
            tokio::time::timeout(BLOB_READ_TIMEOUT, async {
                let entry = read_blob_entry(&self.repository, digest)
                    .await
                    .map_err(blob_failure)?;
                let length = std::num::NonZeroU64::new(entry.expected().byte_length())
                    .filter(|length| length.get() <= signalbox_blob_store::MAX_BLOB_RANGE_BYTES)
                    .ok_or_else(|| failure("blob exceeds the existing direct-read range"))?;
                read_blob_chunk(&self.stores, &entry, 0, length)
                    .await
                    .map_err(blob_failure)
            })
            .await
            .map_err(infrastructure_failure)?
        })
    }
}

#[derive(Debug)]
struct ObservedJudge {
    model: Arc<dyn ApprovalJudgeModel>,
    call: Mutex<Option<String>>,
}
impl ApprovalJudgeModel for ObservedJudge {
    fn prepare<'a>(
        &'a self,
        request: ApprovalJudgeModelRequest,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<PreparedApprovalJudgeModelCall, ApprovalJudgeModelError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            *self
                .call
                .lock()
                .map_err(|_| ApprovalJudgeModelError::PreparationDefect)? =
                Some(request.call.into_uuid().to_string());
            self.model.prepare(request).await
        })
    }
}

fn eval_case(case: &Case) -> ApprovalJudgeEvalCase {
    match case {
        Case::Offline(case) => ApprovalJudgeEvalCase {
            name: case.id.clone(),
            tool: case.request.tool.clone(),
            arguments: case.request.arguments.clone(),
            goal: case.request.commissioned_goal.clone(),
            template: case.request.session_template.clone(),
            system_prompt: case.request.frozen_system_prompt.clone(),
            dispatch: None,
        },
        Case::Live(case) => ApprovalJudgeEvalCase {
            name: case.name.clone(),
            tool: case.tool.clone(),
            arguments: case.arguments.clone(),
            goal: case.goal.clone(),
            template: case.template.clone(),
            system_prompt: case.system_prompt.clone(),
            dispatch: case
                .dispatch
                .as_ref()
                .map(|fence| ApprovalJudgeEvalDispatchFence {
                    repository: fence.repository.clone(),
                    pull_request: fence.pull_request,
                    head_sha: fence.head_sha.clone(),
                    head_repository: fence.head_repository.clone(),
                    head_branch: fence.head_branch.clone(),
                    base_branch: fence.base_branch.clone(),
                }),
        },
    }
}

fn usage_record(usage: TokenUsage) -> Usage {
    Usage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_creation_input_tokens: usage.cache_creation_input_tokens,
        cache_read_input_tokens: usage.cache_read_input_tokens,
    }
}

fn judge_failure(error: &ApprovalJudgeModelError) -> &'static str {
    match error {
        ApprovalJudgeModelError::UnconfiguredTarget => "unconfigured_target",
        ApprovalJudgeModelError::InvalidContract => "invalid_contract",
        ApprovalJudgeModelError::CancelledBeforeSend => "cancelled_before_send",
        ApprovalJudgeModelError::PreparationFailed => "preparation_failed",
        ApprovalJudgeModelError::PreparationDefect => "preparation_defect",
        ApprovalJudgeModelError::AuthorizationMismatch => "authorization_mismatch",
        ApprovalJudgeModelError::PreparationCorrelationMismatch => {
            "preparation_correlation_mismatch"
        }
        ApprovalJudgeModelError::CorrelationMismatch(_) => "correlation_mismatch",
        ApprovalJudgeModelError::Refused(..) => "refused",
        ApprovalJudgeModelError::ProviderError(..) => "provider_error",
        ApprovalJudgeModelError::CancellationConfirmed(_) => "cancellation_confirmed",
        ApprovalJudgeModelError::ProvenUnsent => "proven_unsent",
        ApprovalJudgeModelError::BoundaryLoss(..) => "boundary_loss",
        ApprovalJudgeModelError::ProviderTargetSubstituted(..) => "provider_target_substituted",
        ApprovalJudgeModelError::IncompleteDecision(..) => "incomplete_decision",
        ApprovalJudgeModelError::InvalidDecision(..) => "invalid_decision",
    }
}

// The live scorecard uses FNV-1a independently of the SHA-256 corpus identity.
fn stable_digest(bytes: &[u8]) -> String {
    let mut hash = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d_u128;
    for byte in bytes {
        hash ^= u128::from(*byte);
        hash = hash.wrapping_mul(0x0000_0000_0100_0000_0000_0000_0000_013b);
    }
    format!("fnv1a128:{hash:032x}")
}

#[derive(Debug)]
enum EvalFailure {
    Rejected(LiveDeliveryFailure),
    Infrastructure(LiveDeliveryFailure),
}

fn failure(error: impl std::fmt::Display) -> EvalFailure {
    EvalFailure::Rejected(LiveDeliveryFailure::new(error.to_string()))
}

fn infrastructure_failure(error: impl std::fmt::Display) -> EvalFailure {
    EvalFailure::Infrastructure(LiveDeliveryFailure::new(error.to_string()))
}

fn blob_failure(error: BlobReadError) -> EvalFailure {
    let message = format!("blob read: {error:?}");
    match error {
        BlobReadError::Unavailable
        | BlobReadError::Integrity
        | BlobReadError::Missing
        | BlobReadError::Corrupt => infrastructure_failure(message),
        BlobReadError::NotFound | BlobReadError::RangeOutOfBounds { .. } => failure(message),
    }
}

#[cfg(test)]
mod tests;

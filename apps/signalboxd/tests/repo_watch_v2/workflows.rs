//! Host/module receipt adoption and checked repository-watch effects.
//! Exercises docs/spec/workflows.md and docs/spec/repo-watch.md.

#[path = "workflows/production.rs"]
mod production;

#[path = "workflows/boundaries.rs"]
mod boundaries;

use super::*;
use signalbox_domain::{
    InlineFramePayload, ProgramCapability, ProgramRunId,
    program_registration::{ProgramGrants, ProgramRegistrationRequest},
};
use signalbox_module_repo_watch_v2::{
    dispatch::{CommandSubmission, SessionCommandSink},
    workflow::{EvaluationInvocation, EvaluationOutcome, RuleContext},
};
use signalbox_persistence::{
    program_journal::ProgramJournalRepository, program_registration::ProgramRegistrationRepository,
};
use signalbox_workflow_runtime::{
    LiveDeliveryFailure, LiveDeliverySource, WorkflowHost,
    effects::{EffectExecutor, EffectInvocation, EffectRecovery},
};
use signalboxd::workflows::repo_watch::effects::{RepoWatchEffects, RepoWatchRequest};
use std::{future::Future, pin::Pin};

struct NoPrimitives;
impl LiveDeliverySource for NoPrimitives {
    fn next_delivery<'a>(
        &'a mut self,
        _: &'a [signalbox_domain::RequestFrame],
    ) -> Pin<
        Box<dyn Future<Output = Result<signalbox_domain::DeliveryKind, LiveDeliveryFailure>> + 'a>,
    > {
        Box::pin(async { Err(LiveDeliveryFailure::new("unexpected primitive")) })
    }
}

#[derive(Default)]
struct ConflictingSink {
    calls: usize,
    interrupt: bool,
    commands: Vec<Vec<u8>>,
}
impl SessionCommandSink for ConflictingSink {
    type Error = ();
    async fn submit(&mut self, command: SessionCommand) -> Result<CommandSubmission, Self::Error> {
        self.calls += 1;
        self.commands.push(
            FixtureCommandCodec
                .encode(&command)
                .expect("fixture command encoding"),
        );
        if std::mem::take(&mut self.interrupt) {
            return Err(());
        }
        Ok(CommandSubmission::ConflictingReuse)
    }
}

type Effects =
    RepoWatchEffects<FixedDispatchIds, FixtureSessionFactory, FixtureCommandCodec, ConflictingSink>;

struct LoseAnswer<'a>(&'a mut Effects);
impl EffectExecutor for LoseAnswer<'_> {
    fn recovery(&self, request: &signalbox_domain::EffectRequest) -> EffectRecovery {
        self.0.recovery(request)
    }
    fn adopt<'a>(
        &'a mut self,
        invocation: EffectInvocation<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<Option<InlineFramePayload>, LiveDeliveryFailure>> + 'a>>
    {
        self.0.adopt(invocation)
    }
    fn execute<'a>(
        &'a mut self,
        invocation: EffectInvocation<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<InlineFramePayload, LiveDeliveryFailure>> + 'a>> {
        Box::pin(async move {
            self.0.execute(invocation).await?;
            Err(LiveDeliveryFailure::new(
                "fixture loses the answer after module commit",
            ))
        })
    }
}

async fn start(
    pool: &PgPool,
    request: &signalbox_domain::EffectRequest,
    grants: ProgramGrants,
) -> Result<ProgramRunId, Box<dyn Error>> {
    let artifact = format!(
        r#"import {{ effect }} from "@signalbox/program-sdk/v1";
export default async function(input) {{
  const reply = await effect("repo-watch", "{}", input);
  if (reply.kind !== "answer") throw new Error("effect refused");
  return new Uint8Array(reply.payload);
}}"#,
        request.method()
    );
    let registrations = ProgramRegistrationRepository::new(pool.clone());
    let registered = registrations
        .register_user(
            signalbox_domain::ProgramRegistrationId::from_uuid(Uuid::now_v7()),
            ProgramRegistrationRequest {
                name: Uuid::now_v7().to_string(),
                revision: "fixture".into(),
                source: artifact.as_bytes().to_vec(),
                artifact,
                grants,
            },
        )
        .await?;
    Ok(registrations
        .start_run(
            ProgramRunId::from_uuid(Uuid::now_v7()),
            registered.id,
            request.payload().as_bytes(),
        )
        .await?)
}

#[derive(Clone, Copy, Debug)]
enum Case {
    Dispatch,
    Suppression,
    Nonmatch,
}

async fn fixture(
    case: Case,
) -> Result<
    (
        TestDatabase,
        PgPool,
        PgPool,
        Effects,
        RepositorySlug,
        RepoWatchRule,
    ),
    Box<dyn Error>,
> {
    let (database, core, url) = postgres().await?;
    sqlx::query("ALTER ROLE mod_repo_watch PASSWORD 'signalbox-test-only'")
        .execute(&core)
        .await?;
    let module = module_pool(&url).await?;
    let store = RepoWatchStore::new(module.clone());
    let repository = RepositorySlug::try_new("receipt/project".into())?;
    let now = OffsetDateTime::now_utc();
    let initial = dispatch_observation(&repository, 1, now);
    store
        .ingest_observation(
            &store.ingest_baseline(&repository).await?,
            &initial,
            EventProducer::Poll,
            MERGED_RETENTION,
        )
        .await?;
    let rule = RepoWatchRule::try_new(
        RepoWatchRuleId::try_new("ci".into())?,
        RepoWatchRuleVersion::V1,
        RepoWatchMatcherV1::new(RepoWatchMatcherV1Input {
            event_kinds: vec![if matches!(case, Case::Nonmatch) {
                RepoWatchEventKindNameV1::PullRequestOpened
            } else {
                RepoWatchEventKindNameV1::BranchWorkflowRunCompleted
            }],
            repository: Some(repository.clone()),
            ..Default::default()
        }),
        vec![RepoWatchRuleActionV1::DispatchSession {
            template: SessionTemplateName::try_new("watch".into())?,
        }],
        if matches!(case, Case::Nonmatch) {
            RepoWatchSingletonScope::PullRequest
        } else {
            RepoWatchSingletonScope::Repository
        },
        Duration::ZERO,
    )?;
    store
        .reconcile_rules(
            &[RepositoryRuleSet::new(
                &repository,
                std::slice::from_ref(&rule),
            )],
            now,
        )
        .await?;
    let mut effects = Effects {
        store,
        rules: [(repository.clone(), vec![rule.clone()])].into(),
        // Arbitrary fixture namespaces keep dispatch, command and model identities distinct.
        ids: FixedDispatchIds {
            value: 100,
            calls: 0,
        },
        factory: FixtureSessionFactory {
            next_command: 200,
            model: 300,
        },
        codec: FixtureCommandCodec,
        sink: ConflictingSink::default(),
        source: signalbox_session_ownership::LifecycleEventSource::new(core.clone()),
    };
    for attempt in 2..=if matches!(case, Case::Suppression) {
        3
    } else {
        2
    } {
        effects
            .store
            .ingest_observation(
                &effects.store.ingest_baseline(&repository).await?,
                &dispatch_observation(&repository, attempt, now),
                EventProducer::Poll,
                MERGED_RETENTION,
            )
            .await?;
        if matches!(case, Case::Suppression) && attempt == 2 {
            effects
                .store
                .evaluate_next(
                    &repository,
                    &rule,
                    &mut effects.ids,
                    &mut effects.factory,
                    &mut effects.codec,
                    now,
                )
                .await
                .expect("seed occupied singleton");
        }
    }
    Ok((database, core, module, effects, repository, rule))
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires disposable PostgreSQL"]
async fn workflow_evaluation_quarantines_undecodable_event_and_advances()
-> Result<(), Box<dyn Error>> {
    let (_database, core, module, mut effects, repository, rule) = fixture(Case::Dispatch).await?;
    let poisoned = effects
        .store
        .next_rule_context(&repository, &rule)
        .await?
        .expect("first eligible context");
    sqlx::query("UPDATE gh_event SET normalized_payload = $2 WHERE event_id = $1")
        .bind(poisoned.event.id().into_uuid())
        .bind(b"not json".as_slice())
        .execute(&module)
        .await?;
    let decode_error: Option<String> =
        sqlx::query_scalar("SELECT decode_error FROM gh_event WHERE event_id = $1")
            .bind(poisoned.event.id().into_uuid())
            .fetch_one(&module)
            .await?;
    assert!(decode_error.is_none());
    let now = OffsetDateTime::now_utc();
    effects
        .store
        .ingest_observation(
            &effects.store.ingest_baseline(&repository).await?,
            &dispatch_observation(&repository, 3, now),
            EventProducer::Poll,
            MERGED_RETENTION,
        )
        .await?;

    let journal = ProgramJournalRepository::new(core.clone());
    let host = WorkflowHost::new(journal.clone());
    let next = RepoWatchRequest::NextRuleEvent {
        repository: repository.clone(),
        rule: rule.id().clone(),
    }
    .encode()?;
    let read_run = start(
        &core,
        &next,
        ProgramGrants::new([ProgramCapability::RepoWatch]),
    )
    .await?;
    host.execute_registered(read_run, &mut NoPrimitives, &mut effects)
        .await?;
    let context = RuleContext::decode(
        journal
            .load(read_run)
            .await?
            .expect("read journal")
            .result()
            .expect("read result")
            .as_bytes(),
    )
    .expect("checked context");
    assert!(context.ordinal > poisoned.ordinal);

    let commit = RepoWatchRequest::CommitEvaluation {
        effect: Uuid::now_v7(),
        plan: context.plan(),
        context: Box::new(context),
    }
    .encode()?;
    let commit_run = start(
        &core,
        &commit,
        ProgramGrants::new([ProgramCapability::RepoWatch]),
    )
    .await?;
    host.execute_registered(commit_run, &mut NoPrimitives, &mut effects)
        .await?;
    let receipt = effects
        .store
        .evaluation_receipts()
        .await?
        .pop()
        .expect("committed evaluation");
    assert!(matches!(
        EvaluationOutcome::decode(&receipt.result),
        Some(EvaluationOutcome::Dispatched(_))
    ));
    let decode_error: Option<String> =
        sqlx::query_scalar("SELECT decode_error FROM gh_event WHERE event_id = $1")
            .bind(poisoned.event.id().into_uuid())
            .fetch_one(&module)
            .await?;
    assert_eq!(
        decode_error.as_deref(),
        Some("repository-watch retained event is invalid")
    );
    let readable: bool = sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1 FROM gh_readable_event WHERE event_id = $1)",
    )
    .bind(poisoned.event.id().into_uuid())
    .fetch_one(&module)
    .await?;
    assert!(!readable, "the readable event view excludes quarantine");
    module.close().await;
    core.close().await;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires disposable PostgreSQL"]
async fn workflow_commit_quarantines_event_corrupted_after_context_read()
-> Result<(), Box<dyn Error>> {
    let (_database, core, module, mut effects, repository, rule) = fixture(Case::Dispatch).await?;
    let context = effects
        .store
        .next_rule_context(&repository, &rule)
        .await?
        .expect("first eligible context");
    sqlx::query("UPDATE gh_event SET normalized_payload = $2 WHERE event_id = $1")
        .bind(context.event.id().into_uuid())
        .bind(b"not json".as_slice())
        .execute(&module)
        .await?;
    let now = OffsetDateTime::now_utc();
    effects
        .store
        .ingest_observation(
            &effects.store.ingest_baseline(&repository).await?,
            &dispatch_observation(&repository, 3, now),
            EventProducer::Poll,
            MERGED_RETENTION,
        )
        .await?;
    let effect = Uuid::now_v7();
    let input = b"commit-time quarantine";
    let plan = context.plan();
    let result = effects
        .store
        .commit_evaluation(
            EvaluationInvocation {
                effect,
                input,
                context: &context,
                plan: &plan,
                now,
            },
            &mut effects.ids,
            &mut effects.factory,
            &mut effects.codec,
        )
        .await;
    assert!(matches!(result, Err(StoreError::InvalidRetainedEvent)));
    let decode_error: Option<String> =
        sqlx::query_scalar("SELECT decode_error FROM gh_event WHERE event_id = $1")
            .bind(context.event.id().into_uuid())
            .fetch_one(&module)
            .await?;
    assert_eq!(
        decode_error.as_deref(),
        Some("repository-watch retained event is invalid")
    );
    let retry = effects
        .store
        .commit_evaluation(
            EvaluationInvocation {
                effect,
                input,
                context: &context,
                plan: &plan,
                now,
            },
            &mut effects.ids,
            &mut effects.factory,
            &mut effects.codec,
        )
        .await;
    assert!(matches!(retry, Err(StoreError::WorkflowInputRejected)));
    let successor = effects
        .store
        .next_rule_context(&repository, &rule)
        .await?
        .expect("successor context");
    assert!(successor.ordinal > context.ordinal);
    module.close().await;
    core.close().await;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires disposable PostgreSQL"]
async fn workflow_evaluations_recover_after_another_run_acknowledges() -> Result<(), Box<dyn Error>>
{
    for case in [Case::Dispatch, Case::Suppression, Case::Nonmatch] {
        let (_database, core, module, mut effects, repository, rule) = fixture(case).await?;
        let journal = ProgramJournalRepository::new(core.clone());
        let host = WorkflowHost::new(journal.clone());
        let next = RepoWatchRequest::NextRuleEvent {
            repository: repository.clone(),
            rule: rule.id().clone(),
        }
        .encode()?;
        let read_run = start(
            &core,
            &next,
            ProgramGrants::new([ProgramCapability::RepoWatch]),
        )
        .await?;
        host.execute_registered(read_run, &mut NoPrimitives, &mut effects)
            .await?;
        let context = RuleContext::decode(
            journal
                .load(read_run)
                .await?
                .expect("read journal")
                .result()
                .expect("read result")
                .as_bytes(),
        )
        .expect("checked context");
        assert_eq!(context.rule, rule);
        let request = RepoWatchRequest::CommitEvaluation {
            effect: Uuid::now_v7(),
            plan: context.plan(),
            context: Box::new(context),
        }
        .encode()?;
        let run = start(
            &core,
            &request,
            ProgramGrants::new([ProgramCapability::RepoWatch]),
        )
        .await?;
        assert!(
            host.execute_registered(run, &mut NoPrimitives, &mut LoseAnswer(&mut effects))
                .await
                .is_err()
        );
        let receipt = effects
            .store
            .evaluation_receipts()
            .await?
            .pop()
            .expect("committed receipt");
        let result = EvaluationOutcome::decode(&receipt.result).expect("checked outcome");
        assert!(
            effects
                .store
                .submission_receipt(receipt.effect, &receipt.input)
                .await
                .is_err(),
            "another method cannot adopt an evaluation identity"
        );
        assert!(
            matches!(
                (case, result),
                (Case::Dispatch, EvaluationOutcome::Dispatched(_))
                    | (Case::Suppression, EvaluationOutcome::Suppressed)
                    | (Case::Nonmatch, EvaluationOutcome::Nonmatch)
            ),
            "{case:?}: {result:?}"
        );
        let commands: Vec<(Uuid, Vec<u8>)> = sqlx::query_as(
            "SELECT command_id, command_payload FROM dispatch_ledger ORDER BY command_id",
        )
        .fetch_all(&module)
        .await?;
        let minted = effects.factory.next_command;
        effects
            .store
            .reconcile_rules(&[], OffsetDateTime::now_utc())
            .await?;
        effects.rules.clear();
        assert!(
            effects
                .acknowledge_receipt(&journal, run, &receipt)
                .await
                .is_err()
        );
        // A successor has another journal identity and adopts the same business receipt.
        let successor = start(
            &core,
            &request,
            ProgramGrants::new([ProgramCapability::RepoWatch]),
        )
        .await?;
        host.execute_registered(successor, &mut NoPrimitives, &mut effects)
            .await?;
        assert_eq!(
            journal
                .load(successor)
                .await?
                .expect("successor journal")
                .result()
                .expect("result")
                .as_bytes(),
            receipt.result
        );
        effects
            .acknowledge_receipt(&journal, successor, &receipt)
            .await?;
        assert!(effects.store.evaluation_receipts().await?.is_empty());
        host.execute_registered(run, &mut NoPrimitives, &mut effects)
            .await?;
        assert_eq!(
            journal
                .load(run)
                .await?
                .expect("original journal")
                .result()
                .expect("original result")
                .as_bytes(),
            receipt.result,
            "another run recovers the exact result after acknowledgement"
        );
        assert!(
            effects
                .store
                .submission_receipt(receipt.effect, &receipt.input)
                .await
                .is_err(),
            "acknowledgement preserves method conflicts"
        );
        assert_eq!(
            effects.factory.next_command, minted,
            "adoption does not mint commands"
        );
        let after: Vec<(Uuid, Vec<u8>)> = sqlx::query_as(
            "SELECT command_id, command_payload FROM dispatch_ledger ORDER BY command_id",
        )
        .fetch_all(&module)
        .await?;
        assert_eq!(after, commands, "adoption preserves exact command bytes");
        let mut changed = receipt.input.clone();
        changed.push(b' ');
        assert!(
            effects
                .store
                .adopt_evaluation(receipt.effect, &changed)
                .await
                .is_err()
        );
        effects
            .acknowledge_receipt(&journal, successor, &receipt)
            .await?;
        assert!(effects.store.evaluation_receipts().await?.is_empty());
        assert!(
            sqlx::query("SELECT * FROM public.program_registration")
                .execute(&module)
                .await
                .is_err(),
            "module role remains isolated"
        );
        module.close().await;
        core.close().await;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires disposable PostgreSQL"]
async fn workflow_submission_recovers_after_another_run_acknowledges() -> Result<(), Box<dyn Error>>
{
    let (_database, core, module, mut effects, repository, rule) = fixture(Case::Dispatch).await?;
    let context = effects
        .store
        .next_rule_context(&repository, &rule)
        .await?
        .expect("next context");
    let commit = RepoWatchRequest::CommitEvaluation {
        effect: Uuid::now_v7(),
        plan: context.plan(),
        context: Box::new(context),
    }
    .encode()?;
    let host = WorkflowHost::new(ProgramJournalRepository::new(core.clone()));
    let commit_run = start(
        &core,
        &commit,
        ProgramGrants::new([ProgramCapability::RepoWatch]),
    )
    .await?;
    host.execute_registered(commit_run, &mut NoPrimitives, &mut effects)
        .await?;
    let receipt = effects
        .store
        .evaluation_receipts()
        .await?
        .pop()
        .expect("evaluation");
    let EvaluationOutcome::Dispatched(dispatch) =
        EvaluationOutcome::decode(&receipt.result).expect("outcome")
    else {
        panic!("expected dispatch");
    };
    effects
        .store
        .reconcile_rules(&[], OffsetDateTime::now_utc())
        .await?;
    effects.rules.clear();
    let request = RepoWatchRequest::SubmitPending {
        effect: Uuid::now_v7(),
        dispatch,
    }
    .encode()?;
    let run = start(
        &core,
        &request,
        ProgramGrants::new([ProgramCapability::RepoWatch]),
    )
    .await?;
    effects.sink.interrupt = true;
    assert!(
        host.execute_registered(run, &mut NoPrimitives, &mut effects)
            .await
            .is_err()
    );
    let pending = effects
        .store
        .submission_receipts()
        .await?
        .pop()
        .expect("submission binding");
    assert!(pending.result.is_none());
    let successor = start(
        &core,
        &request,
        ProgramGrants::new([ProgramCapability::RepoWatch]),
    )
    .await?;
    assert!(
        host.execute_registered(successor, &mut NoPrimitives, &mut LoseAnswer(&mut effects))
            .await
            .is_err()
    );
    assert_eq!(effects.sink.calls, 2);
    assert_eq!(
        effects.sink.commands[0], effects.sink.commands[1],
        "resumption uses the exact retained command"
    );
    host.execute_registered(run, &mut NoPrimitives, &mut effects)
        .await?;
    assert_eq!(
        effects.sink.calls, 2,
        "completed submission adopts without invoking the sink"
    );
    let rejection: String =
        sqlx::query_scalar("SELECT rejection_kind FROM dispatch_ledger WHERE dispatch_ref = $1")
            .bind(dispatch.into_uuid())
            .fetch_one(&module)
            .await?;
    assert_eq!(rejection, "conflicting_reuse");
    let journal = ProgramJournalRepository::new(core.clone());
    let unknown = RepoWatchRequest::SubmitPending {
        effect: Uuid::now_v7(),
        dispatch,
    }
    .encode()?;
    let unanswered = start(
        &core,
        &unknown,
        ProgramGrants::new([ProgramCapability::RepoWatch]),
    )
    .await?;
    journal
        .append_request(
            unanswered,
            None,
            signalbox_domain::RequestKind::Effect(unknown),
        )
        .await?;
    let outcome = host
        .execute_registered(unanswered, &mut NoPrimitives, &mut effects)
        .await?;
    let signalbox_workflow_runtime::ProgramExecutionOutcome::Completed(payload) = outcome else {
        panic!("retained ambiguity answer");
    };
    assert_eq!(
        signalboxd::workflows::repo_watch::effects::SubmissionOutcome::decode(&payload),
        Some(signalboxd::workflows::repo_watch::effects::SubmissionOutcome::Ambiguous)
    );
    assert_eq!(
        effects.sink.calls, 2,
        "an unanswered submission without a binding is not retried"
    );
    let completed = effects
        .store
        .submission_receipts()
        .await?
        .pop()
        .expect("completed binding");
    effects
        .acknowledge_receipt(
            &journal,
            run,
            &signalbox_module_repo_watch_v2::workflow::EffectReceipt {
                effect: completed.effect,
                input: completed.input,
                result: completed.result.expect("completed result"),
            },
        )
        .await?;
    assert!(effects.store.submission_receipts().await?.is_empty());
    let recovered = host
        .execute_registered(successor, &mut NoPrimitives, &mut effects)
        .await?;
    let signalbox_workflow_runtime::ProgramExecutionOutcome::Completed(payload) = recovered else {
        panic!("completed submission remains recoverable after acknowledgement");
    };
    assert_eq!(payload.as_bytes(), b"submitted");
    let fresh = start(
        &core,
        &request,
        ProgramGrants::new([ProgramCapability::RepoWatch]),
    )
    .await?;
    assert_eq!(
        host.execute_registered(fresh, &mut NoPrimitives, &mut effects)
            .await?,
        signalbox_workflow_runtime::ProgramExecutionOutcome::Completed(payload)
    );
    assert_eq!(
        effects.sink.calls, 2,
        "recovery after acknowledgement never resubmits commands"
    );
    assert!(
        effects
            .store
            .adopt_evaluation(completed.effect, request.payload().as_bytes())
            .await
            .is_err(),
        "acknowledgement preserves method conflicts"
    );
    let mut changed = request.payload().as_bytes().to_vec();
    changed.push(b' ');
    assert!(
        effects
            .store
            .submission_receipt(completed.effect, &changed)
            .await
            .is_err(),
        "acknowledgement preserves exact-input conflicts"
    );
    module.close().await;
    core.close().await;
    Ok(())
}

#[cfg(target_os = "linux")]
mod native_recovery {
    use super::*;
    use signalbox_workflow_runtime::native::{
        NativeCatalog, NativeProgram, NativeProgramError, NativeValue, WorkflowContext,
    };

    struct EvaluationInput(signalbox_domain::EffectRequest);
    impl NativeValue for EvaluationInput {
        fn decode(bytes: &[u8]) -> Result<Self, NativeProgramError> {
            let request = signalbox_domain::EffectRequest::new(
                ProgramCapability::RepoWatch,
                "repo.commitEvaluation".into(),
                InlineFramePayload::new(bytes),
            );
            RepoWatchRequest::decode(&request)
                .ok_or_else(|| NativeProgramError::new("invalid evaluation"))?;
            Ok(Self(request))
        }
        fn encode(&self) -> Result<Vec<u8>, NativeProgramError> {
            Ok(self.0.payload().as_bytes().to_vec())
        }
    }
    struct EvaluationResult(Vec<u8>);
    impl NativeValue for EvaluationResult {
        fn decode(bytes: &[u8]) -> Result<Self, NativeProgramError> {
            EvaluationOutcome::decode(bytes)
                .ok_or_else(|| NativeProgramError::new("invalid evaluation outcome"))?;
            Ok(Self(bytes.to_vec()))
        }
        fn encode(&self) -> Result<Vec<u8>, NativeProgramError> {
            Ok(self.0.clone())
        }
    }
    struct CommitProgram;
    impl NativeProgram for CommitProgram {
        type Input = EvaluationInput;
        type Output = EvaluationResult;
        async fn run(
            mut context: WorkflowContext,
            input: EvaluationInput,
        ) -> Result<EvaluationResult, NativeProgramError> {
            EvaluationResult::decode(context.effect(input.0).await?.as_bytes())
        }
    }

    #[tokio::test(flavor = "current_thread")]
    #[ignore = "requires disposable PostgreSQL"]
    async fn retired_native_run_leaves_an_adoptable_receipt_for_a_successor()
    -> Result<(), Box<dyn Error>> {
        use signalbox_domain::program_registration::{
            NativeProgramRegistrationRequest, ProgramExecutable,
        };
        let (_database, core, module, mut effects, repository, rule) =
            fixture(Case::Dispatch).await?;
        let context = effects
            .store
            .next_rule_context(&repository, &rule)
            .await?
            .expect("context");
        let request = RepoWatchRequest::CommitEvaluation {
            effect: Uuid::now_v7(),
            plan: context.plan(),
            context: Box::new(context),
        }
        .encode()?;
        let mut catalog = NativeCatalog::new()?;
        catalog.insert::<CommitProgram>("repo-evaluation-fixture".into(), "one".into())?;
        let ProgramExecutable::Native {
            entry,
            revision: native_revision,
            binary_digest,
        } = catalog
            .executable("repo-evaluation-fixture", "one")
            .expect("entry")
        else {
            panic!("native executable");
        };
        let registrations = ProgramRegistrationRepository::new(core.clone());
        let registration = registrations
            .register_native_user(
                signalbox_domain::ProgramRegistrationId::from_uuid(Uuid::now_v7()),
                NativeProgramRegistrationRequest {
                    name: "evaluation".into(),
                    revision: "one".into(),
                    entry,
                    native_revision,
                    binary_digest,
                    grants: ProgramGrants::new([ProgramCapability::RepoWatch]),
                },
            )
            .await?;
        let run = registrations
            .start_run(
                ProgramRunId::from_uuid(Uuid::now_v7()),
                registration.id,
                request.payload().as_bytes(),
            )
            .await?;
        let journal = ProgramJournalRepository::new(core.clone());
        let host = WorkflowHost::new(journal.clone()).with_native_catalog(catalog);
        assert!(
            host.execute_registered(run, &mut NoPrimitives, &mut LoseAnswer(&mut effects))
                .await
                .is_err()
        );
        let replacement = WorkflowHost::new(journal.clone());
        assert!(matches!(
            replacement
                .execute_registered(run, &mut NoPrimitives, &mut effects)
                .await?,
            signalbox_workflow_runtime::ProgramExecutionOutcome::Faulted(
                signalbox_domain::ProgramFault::ContractRetired(_)
            )
        ));
        let receipt = effects
            .store
            .evaluation_receipts()
            .await?
            .pop()
            .expect("receipt survives retirement");
        let successor = start(
            &core,
            &request,
            ProgramGrants::new([ProgramCapability::RepoWatch]),
        )
        .await?;
        replacement
            .execute_registered(successor, &mut NoPrimitives, &mut effects)
            .await?;
        effects
            .store
            .ingest_observation(
                &effects.store.ingest_baseline(&repository).await?,
                &dispatch_observation(&repository, 3, OffsetDateTime::now_utc()),
                EventProducer::Poll,
                MERGED_RETENTION,
            )
            .await?;
        assert!(
            effects
                .store
                .next_rule_context(&repository, &rule)
                .await?
                .is_none(),
            "the next retained event waits for receipt adoption"
        );
        effects
            .acknowledge_receipt(&journal, successor, &receipt)
            .await?;
        assert!(
            effects
                .store
                .next_rule_context(&repository, &rule)
                .await?
                .is_some(),
            "successor adoption permits the next event"
        );
        assert_eq!(effects.ids.calls, 1, "successor does not repeat dispatch");
        let next_context = effects
            .store
            .next_rule_context(&repository, &rule)
            .await?
            .expect("next event");
        let next_request = RepoWatchRequest::CommitEvaluation {
            effect: Uuid::now_v7(),
            plan: next_context.plan(),
            context: Box::new(next_context),
        }
        .encode()?;
        let next_run = start(
            &core,
            &next_request,
            ProgramGrants::new([ProgramCapability::RepoWatch]),
        )
        .await?;
        replacement
            .execute_registered(next_run, &mut NoPrimitives, &mut effects)
            .await?;
        let next_receipt = effects
            .store
            .evaluation_receipts()
            .await?
            .pop()
            .expect("next pending receipt");
        assert_ne!(next_receipt.effect, receipt.effect);
        effects
            .store
            .reconcile_rules(&[], OffsetDateTime::now_utc())
            .await?;
        effects.rules.clear();
        let minted = effects.ids.calls;
        let late_successor = start(
            &core,
            &request,
            ProgramGrants::new([ProgramCapability::RepoWatch]),
        )
        .await?;
        let result = replacement
            .execute_registered(late_successor, &mut NoPrimitives, &mut effects)
            .await?;
        assert_eq!(
            result,
            signalbox_workflow_runtime::ProgramExecutionOutcome::Completed(
                InlineFramePayload::new(receipt.result.clone())
            ),
            "fresh equal requests keep the completed result after the cursor changes"
        );
        assert_eq!(
            effects.ids.calls, minted,
            "completed bindings never repeat planning"
        );
        assert_eq!(
            effects
                .store
                .adopt_evaluation(receipt.effect, &receipt.input)
                .await?,
            Some(receipt.result.clone()),
            "lost-answer adoption keeps the completed binding after cursor advancement"
        );
        effects
            .acknowledge_receipt(&journal, late_successor, &receipt)
            .await?;
        assert_eq!(
            effects.store.evaluation_receipts().await?,
            vec![next_receipt],
            "late acknowledgement cannot release another effect's pending slot"
        );
        module.close().await;
        core.close().await;
        Ok(())
    }
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires disposable PostgreSQL"]
async fn workflow_evaluation_revalidates_plans_and_rolls_back_without_a_receipt()
-> Result<(), Box<dyn Error>> {
    let (_database, core, module, mut effects, repository, rule) = fixture(Case::Dispatch).await?;
    let context = effects
        .store
        .next_rule_context(&repository, &rule)
        .await?
        .expect("context");
    let host = WorkflowHost::new(ProgramJournalRepository::new(core.clone()));
    let denied = RepoWatchRequest::CommitEvaluation {
        effect: Uuid::now_v7(),
        plan: context.plan(),
        context: Box::new(context.clone()),
    }
    .encode()?;
    let run = start(&core, &denied, ProgramGrants::new([])).await?;
    assert!(
        host.execute_registered(run, &mut NoPrimitives, &mut effects)
            .await
            .is_err()
    );
    assert_eq!(
        effects.ids.calls, 0,
        "missing grant never reaches the module"
    );
    let wrong_plan = RepoWatchRequest::CommitEvaluation {
        effect: Uuid::now_v7(),
        plan: Vec::new(),
        context: Box::new(context.clone()),
    }
    .encode()?;
    let run = start(
        &core,
        &wrong_plan,
        ProgramGrants::new([ProgramCapability::RepoWatch]),
    )
    .await?;
    assert!(
        host.execute_registered(run, &mut NoPrimitives, &mut effects)
            .await
            .is_err()
    );
    assert_eq!(
        effects.ids.calls, 0,
        "the module rejects altered pure planning before minting"
    );
    sqlx::query("CREATE FUNCTION reject_evaluation_receipt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.effect_id IS NOT NULL THEN RAISE EXCEPTION 'fixture receipt failure'; END IF; RETURN NEW; END $$").execute(&module).await?;
    sqlx::query("CREATE TRIGGER reject_evaluation_receipt BEFORE INSERT OR UPDATE ON rule_evaluation_cursor FOR EACH ROW EXECUTE FUNCTION reject_evaluation_receipt()").execute(&module).await?;
    let request = RepoWatchRequest::CommitEvaluation {
        effect: Uuid::now_v7(),
        plan: context.plan(),
        context: Box::new(context),
    }
    .encode()?;
    let run = start(
        &core,
        &request,
        ProgramGrants::new([ProgramCapability::RepoWatch]),
    )
    .await?;
    assert!(
        host.execute_registered(run, &mut NoPrimitives, &mut effects)
            .await
            .is_err()
    );
    let commands: i64 = sqlx::query_scalar("SELECT count(*) FROM dispatch_ledger")
        .fetch_one(&module)
        .await?;
    let cursors: i64 = sqlx::query_scalar("SELECT count(*) FROM rule_evaluation_cursor")
        .fetch_one(&module)
        .await?;
    assert_eq!(commands, 0, "receipt failure rolls back commands");
    assert_eq!(cursors, 0, "receipt failure rolls back the cursor");
    sqlx::query("DROP TRIGGER reject_evaluation_receipt ON rule_evaluation_cursor")
        .execute(&module)
        .await?;
    host.execute_registered(run, &mut NoPrimitives, &mut effects)
        .await?;
    assert_eq!(effects.store.evaluation_receipts().await?.len(), 1);
    module.close().await;
    core.close().await;
    Ok(())
}

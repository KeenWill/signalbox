//! Cross-crate test construction for the GitHub executors' evidence path.
//!
//! `ToolExecutionInvocation` has no public constructor: `signalbox-application`
//! mints one only while running a batch, so a test outside that crate cannot
//! call an executor's `execute` directly. Reaching the evidence-producing path
//! therefore means driving the real `ToolExecutionService` over a reconstituted
//! batch, and this module is the minimum needed to do that.
//!
//! It deliberately mirrors `crates/tools-web/src/web_search/test_service_support.rs`,
//! which exists for the same reason. Keep the two shaped alike: a second idiom
//! for the same problem is how the next tool crate ends up inventing a third.

use std::sync::{Arc, Mutex};

use signalbox_application::{
    CorrelatedDurableChildWait, CorrelatedToolExecutorEvidence, InProcessToolDispatchGate,
    PrepareToolContinuationOutcome, RetainedToolAttemptObservationStatus,
    ToolAttemptAuthorizationStatus, ToolContinuationIdentities, ToolCrashClosureIdentities,
    ToolExecutionInvocation, ToolExecutionService, ToolExecutionTransaction, ToolExecutor,
    ToolExecutorEvidence, UuidV7ToolLoopIdGenerator,
};
use signalbox_domain::{
    AcceptedInputId, ContextFrontierId, CorrelatedToolAttemptObservation, CurrentToolAttempt,
    EndedToolAttempt, ModelCallId, NormalizedToolArguments,
    ResolvedContextFrontierReconstitutionInput, SemanticTranscriptEntryId, SessionId,
    ToolApprovalResolutionReconstitutionInput, ToolAttemptCrashOutcome,
    ToolAttemptDispatchCorrelation, ToolAttemptId, ToolAttemptReconstitutionInput,
    ToolAttemptReconstitutionState, ToolBatchPhaseReconstitutionInput,
    ToolBatchReconstitutionInput, ToolDispatchGeneration, ToolEffectClass, ToolExecutionError,
    ToolName, ToolRequestId, ToolRequestOrdinal, ToolRequestReconstitutionInput, TurnAttemptId,
    TurnId,
};
use signalbox_model_runtime::{CredentialAccess, CredentialAccessError, CredentialValue};

use super::*;

pub(crate) const FIXTURE_TOKEN: &str = "github_pat_synthetic_fixture_secret";

pub(crate) const FIXTURE_REPOSITORY: &str = "KeenWill/signalbox";

const FIXTURE_TITLE: &str = "Repair the failing invariant";

const FIXTURE_BODY: &str = "Synthetic repair body";

const FIXTURE_HEAD: &str = "agent/fix-invariant";

const FIXTURE_BASE: &str = "main";

const SESSION_IDENTITY: u128 = 0x9001;
const TURN_IDENTITY: u128 = 0x9002;
const PRODUCING_CALL_IDENTITY: u128 = 0x9003;
const REQUEST_IDENTITY: u128 = 0x9004;
const ATTEMPT_IDENTITY: u128 = 0x9005;
const ISSUING_ATTEMPT_IDENTITY: u128 = 0x9006;
const FRONTIER_IDENTITY: u128 = 0x9007;

/// Credentials that always resolve to the synthetic fixture token.
#[derive(Clone, Copy, Debug)]
pub(crate) struct FixtureCredentials;

impl CredentialAccess for FixtureCredentials {
    async fn resolve(
        &self,
        _reference: &CredentialReference,
    ) -> Result<CredentialValue, CredentialAccessError> {
        Ok(CredentialValue::new(FIXTURE_TOKEN.as_bytes().to_vec()))
    }
}

/// What one driven creation attempt produced.
///
/// The evidence is the executor's own return value, captured before the
/// service converts it into a durable observation, so a test can assert the
/// exact `ToolExecutorEvidence` variant and detail rather than whatever the
/// commit path happened to persist.
pub(crate) struct CreateExecutionOutcome {
    /// Evidence the executor bound to its invocation, if it returned any.
    pub(crate) evidence: Option<ToolExecutorEvidence>,
    /// Whether the service committed that evidence rather than erroring.
    pub(crate) committed: bool,
}

/// Drives one prepared `github_pull_request_create` attempt through the real
/// execution service against `transport`.
pub(crate) async fn create_pull_request_evidence<Transport>(
    transport: Transport,
) -> CreateExecutionOutcome
where
    Transport: GitHubTransport + Send,
{
    let repository = GitHubRepository::try_from(String::from(FIXTURE_REPOSITORY))
        .expect("fixture repository is admitted");
    let (catalog, executor) = GitHubPullRequestCreateTools::try_new(
        FixtureCredentials,
        transport,
        GitHubEgressPolicy::github_api_only(),
        repository,
    )
    .expect("creation suite constructs")
    .into_parts();
    let recorded = Arc::new(Mutex::new(None));
    let executor = RecordingExecutor {
        inner: executor,
        recorded: Arc::clone(&recorded),
    };
    let batch = prepared_create_batch();
    let mut service = ToolExecutionService::new(
        UuidV7ToolLoopIdGenerator,
        CreateFixtureTransaction {
            batch: batch.clone(),
        },
        catalog,
        executor,
        InProcessToolDispatchGate::default(),
    );
    let committed = service.execute(batch.session(), batch.turn()).await.is_ok();
    let evidence = recorded
        .lock()
        .expect("recorded evidence lock is available")
        .clone();

    CreateExecutionOutcome {
        evidence,
        committed,
    }
}

/// Captures the executor's evidence on its way back to the service.
struct RecordingExecutor<Executor> {
    inner: Executor,
    recorded: Arc<Mutex<Option<ToolExecutorEvidence>>>,
}

impl<Executor> ToolExecutor for RecordingExecutor<Executor>
where
    Executor: ToolExecutor<Error = GitHubExecutorError> + Send,
{
    type Error = GitHubExecutorError;

    async fn execute(
        &mut self,
        invocation: ToolExecutionInvocation,
    ) -> Result<CorrelatedToolExecutorEvidence, Self::Error> {
        let result = self.inner.execute(invocation).await;
        if let Ok(correlated) = &result {
            *self
                .recorded
                .lock()
                .expect("recorded evidence lock is available") =
                Some(correlated.evidence().clone());
        }
        result
    }
}

/// One prepared external-effect creation attempt awaiting dispatch.
fn prepared_create_batch() -> signalbox_domain::ToolBatch {
    let session = SessionId::from_uuid(uuid::Uuid::from_u128(SESSION_IDENTITY));
    let turn = TurnId::from_uuid(uuid::Uuid::from_u128(TURN_IDENTITY));
    let producing_call = ModelCallId::from_uuid(uuid::Uuid::from_u128(PRODUCING_CALL_IDENTITY));
    let arguments = serde_json::json!({
        "title": FIXTURE_TITLE,
        "body": FIXTURE_BODY,
        "head": FIXTURE_HEAD,
        "base": FIXTURE_BASE,
    })
    .to_string();
    let request = ToolRequestReconstitutionInput::new(
        ToolRequestId::from_uuid(uuid::Uuid::from_u128(REQUEST_IDENTITY)),
        session,
        turn,
        producing_call,
        ToolRequestOrdinal::from_u32(0),
        ToolName::try_new(String::from(PULL_REQUEST_CREATE_NAME)).expect("fixture name is valid"),
        NormalizedToolArguments::try_from_provider_text(arguments)
            .expect("fixture creation arguments are valid"),
    )
    .into_request();
    let turn_attempt = TurnAttemptId::from_uuid(uuid::Uuid::from_u128(ISSUING_ATTEMPT_IDENTITY));
    let approval = ToolApprovalResolutionReconstitutionInput::policy_auto(request.id())
        .reconstitute()
        .expect("policy approval fixture is valid");
    // The catalog declares creation `ExternalEffect`, and the invocation is
    // only built when the prepared attempt agrees, so this class is fixed by
    // the production declaration rather than chosen here.
    let attempt = ToolAttemptReconstitutionInput::new(
        ToolAttemptId::from_uuid(uuid::Uuid::from_u128(ATTEMPT_IDENTITY)),
        request.id(),
        session,
        turn,
        turn_attempt,
        ToolEffectClass::ExternalEffect,
        ToolDispatchGeneration::first(),
        ToolAttemptReconstitutionState::Prepared,
    )
    .reconstitute()
    .expect("prepared attempt fixture is valid");
    let frontier = ResolvedContextFrontierReconstitutionInput::new(
        session,
        ContextFrontierId::from_uuid(uuid::Uuid::from_u128(FRONTIER_IDENTITY)),
        Vec::new(),
    )
    .reconstitute()
    .expect("empty frontier fixture is valid");

    ToolBatchReconstitutionInput::new(
        session,
        turn,
        producing_call,
        frontier,
        vec![request],
        vec![approval],
        vec![attempt],
        ToolBatchPhaseReconstitutionInput::Executing { turn_attempt },
    )
    .reconstitute()
    .expect("creation batch fixture is valid")
}

/// Serves the one prepared attempt and applies whatever the executor returns.
struct CreateFixtureTransaction {
    batch: signalbox_domain::ToolBatch,
}

impl ToolExecutionTransaction for CreateFixtureTransaction {
    type Error = GitHubExecutorError;

    async fn resume_child_wait(
        &mut self,
        _session: SessionId,
        _turn: TurnId,
        _attempt: TurnAttemptId,
    ) -> Result<bool, Self::Error> {
        panic!("creation fixtures never contain a delegated child wait")
    }

    async fn reread_durable_child_wait(
        &mut self,
        _wait: CorrelatedDurableChildWait,
    ) -> Result<bool, Self::Error> {
        panic!("creation fixtures never park on a delegated child wait")
    }

    async fn reread_durable_completion(
        &mut self,
        _correlation: ToolAttemptDispatchCorrelation,
    ) -> Result<bool, Self::Error> {
        panic!("creation fixtures never report a durable completion")
    }

    async fn load_active_batch(
        &mut self,
        _session: SessionId,
        _turn: TurnId,
    ) -> Result<Option<signalbox_domain::ToolBatch>, Self::Error> {
        Ok(Some(self.batch.clone()))
    }

    async fn prepare_next_attempt(
        &mut self,
        _session: SessionId,
        _turn: TurnId,
        _attempt: ToolAttemptId,
        _effect_class: ToolEffectClass,
    ) -> Result<Option<CurrentToolAttempt>, Self::Error> {
        panic!("the fixture begins with one prepared attempt")
    }

    async fn authorize_attempt(
        &mut self,
        _session: SessionId,
        _turn: TurnId,
        attempt: ToolAttemptId,
    ) -> Result<signalbox_domain::ToolDispatchAuthority, Self::Error> {
        self.batch
            .authorize_dispatch(attempt)
            .map_err(|_| caller_bug())
    }

    async fn reread_ambiguous_authorization(
        &mut self,
        _session: SessionId,
        _turn: TurnId,
        _attempt: ToolAttemptId,
    ) -> Result<ToolAttemptAuthorizationStatus, Self::Error> {
        panic!("fixture authorization is unambiguous")
    }

    async fn commit_preflight_error(
        &mut self,
        _session: SessionId,
        _turn: TurnId,
        _attempt: ToolAttemptId,
        _error: ToolExecutionError,
    ) -> Result<EndedToolAttempt, Self::Error> {
        panic!("fixture arguments pass preflight")
    }

    async fn commit_observation(
        &mut self,
        observation: CorrelatedToolAttemptObservation,
    ) -> Result<EndedToolAttempt, Self::Error> {
        self.batch
            .authorize_attempt(observation.correlation().attempt())
            .map_err(|_| caller_bug())?
            .into_parts()
            .0
            .apply_terminal_observation(observation)
            .map_err(|_| caller_bug())
    }

    async fn reread_observation(
        &mut self,
        _observation: &CorrelatedToolAttemptObservation,
    ) -> Result<RetainedToolAttemptObservationStatus, Self::Error> {
        Ok(RetainedToolAttemptObservationStatus::Pending)
    }

    async fn classify_crash_loss<NextTurn>(
        &mut self,
        _session: SessionId,
        _turn: TurnId,
        _attempt: ToolAttemptId,
        _identities: ToolCrashClosureIdentities,
        _next_turn: NextTurn,
    ) -> Result<ToolAttemptCrashOutcome, Self::Error>
    where
        NextTurn: FnMut(AcceptedInputId) -> TurnId + Send,
    {
        // Reached, not dead: an executor error the service cannot resolve into
        // evidence routes the attempt here, because an external effect that may
        // already have happened must be classified rather than assumed. The
        // fixture declines to classify, which surfaces as the service error the
        // ambiguous case asserts on.
        Err(infrastructure(CommitOutcome::Ambiguous))
    }

    async fn prepare_continuation<NextSteering>(
        &mut self,
        _session: SessionId,
        _turn: TurnId,
        _producing_call: ModelCallId,
        _identities: ToolContinuationIdentities,
        _next_steering: NextSteering,
    ) -> Result<PrepareToolContinuationOutcome, Self::Error>
    where
        NextSteering: FnMut(AcceptedInputId) -> (SemanticTranscriptEntryId, TurnId) + Send,
    {
        panic!("the fixture has one prepared attempt")
    }
}

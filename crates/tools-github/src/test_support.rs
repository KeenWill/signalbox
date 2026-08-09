//! GitHub-specific construction for the creation executors' evidence path.
//!
//! The machinery for driving an executor through the real `ToolExecutionService`
//! — the prepared batch, the serving transaction, the evidence recorder — is
//! shared with every other provider-adapter crate and lives behind
//! `signalbox_application`'s `test-support` feature. What stays here is what is
//! actually GitHub's: the credential source, the catalog and transport wiring,
//! and this suite's fixture values.

use signalbox_application::{
    FixtureToolExecutionTransaction, FixtureTransactionFailures, InProcessToolDispatchGate,
    PreparedAttemptApproval, PreparedAttemptIdentities, PreparedAttemptProposal,
    RecordingToolExecutor, ToolExecutionService, ToolExecutionServiceError,
    ToolExecutionServiceOutcome, ToolExecutorEvidence, UuidV7ToolLoopIdGenerator,
    prepared_single_attempt_batch,
};
use signalbox_domain::{
    ContextFrontierId, DurableCommandId, ModelCallId, NormalizedToolArguments, SessionId,
    ToolAttemptId, ToolEffectClass, ToolName, ToolRequestId, TurnAttemptId, TurnId,
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
const APPROVAL_IDENTITY: u128 = 0x9008;

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

/// Exactly what the creation service returned, in the shapes it distinguishes.
pub(crate) type CreateServiceResult = Result<
    ToolExecutionServiceOutcome,
    ToolExecutionServiceError<GitHubExecutorError, GitHubExecutorError>,
>;

/// What one driven creation attempt produced.
pub(crate) struct CreateExecutionOutcome {
    /// Evidence the executor bound to its invocation, if it returned any.
    pub(crate) evidence: Option<ToolExecutorEvidence>,
    /// The service's own result, retained whole rather than reduced to a bit.
    ///
    /// "No evidence and not committed" is the shape of *several* different
    /// failures — a definite executor error, a dispatch failure before
    /// classification is ever reached — so a boolean cannot distinguish the
    /// commit-ambiguous classification the ambiguity tests are about from a
    /// regression that lost it. The error carries the executor's own error
    /// nested inside it, which is where that classification lives.
    pub(crate) result: CreateServiceResult,
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
    let (executor, recorded) = RecordingToolExecutor::new(executor);
    let batch = prepared_create_batch();
    let mut service = ToolExecutionService::new(
        UuidV7ToolLoopIdGenerator,
        FixtureToolExecutionTransaction::new(
            batch.clone(),
            FixtureTransactionFailures {
                domain_rejection: caller_bug(),
                declined_crash_classification: infrastructure(CommitOutcome::Ambiguous),
            },
        ),
        catalog,
        executor,
        InProcessToolDispatchGate::default(),
    );
    let result = service.execute(batch.session(), batch.turn()).await;

    CreateExecutionOutcome {
        evidence: recorded.take(),
        result,
    }
}

/// One prepared external-effect creation attempt awaiting dispatch.
fn prepared_create_batch() -> signalbox_domain::ToolBatch {
    let arguments = serde_json::json!({
        "title": FIXTURE_TITLE,
        "body": FIXTURE_BODY,
        "head": FIXTURE_HEAD,
        "base": FIXTURE_BASE,
    })
    .to_string();

    prepared_single_attempt_batch(
        PreparedAttemptIdentities {
            session: SessionId::from_uuid(uuid::Uuid::from_u128(SESSION_IDENTITY)),
            turn: TurnId::from_uuid(uuid::Uuid::from_u128(TURN_IDENTITY)),
            producing_call: ModelCallId::from_uuid(uuid::Uuid::from_u128(PRODUCING_CALL_IDENTITY)),
            request: ToolRequestId::from_uuid(uuid::Uuid::from_u128(REQUEST_IDENTITY)),
            attempt: ToolAttemptId::from_uuid(uuid::Uuid::from_u128(ATTEMPT_IDENTITY)),
            issuing_turn_attempt: TurnAttemptId::from_uuid(uuid::Uuid::from_u128(
                ISSUING_ATTEMPT_IDENTITY,
            )),
            frontier: ContextFrontierId::from_uuid(uuid::Uuid::from_u128(FRONTIER_IDENTITY)),
        },
        PreparedAttemptProposal {
            name: ToolName::try_new(String::from(PULL_REQUEST_CREATE_NAME))
                .expect("fixture name is valid"),
            arguments: NormalizedToolArguments::try_from_provider_text(arguments)
                .expect("fixture creation arguments are valid"),
            // The catalog declares creation `ExternalEffect`, and the invocation
            // is only built when the prepared attempt agrees, so this class is
            // fixed by the production declaration rather than chosen here.
            effect_class: ToolEffectClass::ExternalEffect,
            // Creation is declared `ToolPermissionDefault::Confirm`, so policy
            // alone never admits it; only an explicit decision reaches
            // dispatch, and that is the path these tests must drive.
            approval: PreparedAttemptApproval::UserConfirmation {
                command: DurableCommandId::from_uuid(uuid::Uuid::from_u128(APPROVAL_IDENTITY)),
            },
        },
    )
}

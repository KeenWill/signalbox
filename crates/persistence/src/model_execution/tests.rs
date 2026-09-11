use std::{borrow::Cow, collections::BTreeSet, error::Error, fmt, io, time::Duration};

use signalbox_application::{ClassifyOperatorFailure, OperatorFailureClass};
use signalbox_domain::{
    FastMode, ModelCallId, ProviderModelCallFailureCause, ProviderModelIdentity,
    ResolvedProviderTarget, TurnId,
};
use sqlx::{
    error::{DatabaseError, ErrorKind},
    types::Uuid,
};

use super::credential_pool::remap_preserves_preflight_limits;
use super::delegation_lock::delegation_terminal_relation_decode_error;
use super::persist_tool_round::MAX_AVAILABILITY_BACKOFF;
use super::persist_tool_round::availability_retry_backoff;
use super::persist_tool_round::is_same_credential_retry_cause;
use super::reread::StoredTerminalFrontierMember;
use super::reread::completed_terminal_frontier_matches;
use super::reread::failed_terminal_frontier_matches;
use super::reread::record_reclassified_turn_candidate;
use super::{
    ModelCallCorruption, ModelCallIdentityCollision, ModelCallRepositoryError,
    ToolContinuationUsageLimit, commit_failure_is_ambiguous,
};

#[test]
fn input_failure_reports_the_tool_invariant_without_changing_its_class() {
    let cause = crate::tool_loop::ToolLoopRepositoryError::Corruption(
        crate::tool_loop::ToolLoopCorruption::Inconsistent("tool result payload"),
    );
    let execution = super::prepared::map_tool_evidence_error(cause);
    assert_eq!(
        execution.operator_failure_class(),
        OperatorFailureClass::FailClosedCorruption,
    );
    let failure = crate::submit_input::SubmitInputRepositoryError::from(execution);
    assert_eq!(
        failure.to_string(),
        "SubmitInput model execution failed: inconsistent model-call execution tool result payload",
    );
}

#[test]
fn remapped_call_rejects_missing_preparation_limit_evidence() {
    let target =
        ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(Uuid::from_u128(1)));
    let current = ToolContinuationUsageLimit::new(target, FastMode::Enabled, 10, 100);

    assert!(!remap_preserves_preflight_limits(None, Some(current)));
}

#[test]
fn same_credential_retry_causes_are_closed() {
    for cause in [
        ProviderModelCallFailureCause::RateLimited,
        ProviderModelCallFailureCause::Overloaded,
        ProviderModelCallFailureCause::ProviderInternal,
    ] {
        assert!(is_same_credential_retry_cause(cause));
    }
    for cause in [
        ProviderModelCallFailureCause::CredentialRejected,
        ProviderModelCallFailureCause::PermissionDenied,
        ProviderModelCallFailureCause::InvalidRequest,
        ProviderModelCallFailureCause::TargetNotFound,
        ProviderModelCallFailureCause::RequestTooLarge,
        ProviderModelCallFailureCause::QuotaExhausted,
        ProviderModelCallFailureCause::Unrecognized,
    ] {
        assert!(!is_same_credential_retry_cause(cause));
    }
}

#[test]
fn rate_limit_backoff_is_jittered_inside_the_exponential_window() {
    let delay = availability_retry_backoff(
        ProviderModelCallFailureCause::RateLimited,
        None,
        3,
        ModelCallId::from_uuid(Uuid::from_u128(17)),
    );
    assert!(delay >= Duration::from_secs(2));
    assert!(delay < Duration::from_secs(6));
}

#[test]
fn provider_retry_after_is_a_minimum_until_the_cap() {
    let delay = availability_retry_backoff(
        ProviderModelCallFailureCause::Overloaded,
        Some(Duration::from_secs(47)),
        1,
        ModelCallId::from_uuid(Uuid::from_u128(18)),
    );
    assert_eq!(delay, Duration::from_secs(47));
}

#[test]
fn provider_retry_after_is_capped_and_quota_rotation_is_immediate() {
    let capped = availability_retry_backoff(
        ProviderModelCallFailureCause::RateLimited,
        Some(Duration::from_secs(600)),
        1,
        ModelCallId::from_uuid(Uuid::from_u128(19)),
    );
    assert_eq!(capped, MAX_AVAILABILITY_BACKOFF);
    let quota = availability_retry_backoff(
        ProviderModelCallFailureCause::QuotaExhausted,
        Some(Duration::from_secs(15)),
        1,
        ModelCallId::from_uuid(Uuid::from_u128(20)),
    );
    assert_eq!(quota, Duration::ZERO);
}

#[test]
fn delegated_terminal_relation_decode_failure_is_corruption() {
    let error = sqlx::Error::ColumnDecode {
        index: String::from("parent_session_id"),
        source: Box::new(io::Error::new(
            io::ErrorKind::InvalidData,
            "fixture UUID decode failure",
        )),
    };

    assert!(matches!(
        delegation_terminal_relation_decode_error(error),
        ModelCallRepositoryError::Corruption(ModelCallCorruption::Inconsistent(
            "delegated terminal relationship identity"
        ))
    ));
}

/// docs/spec/model-call-execution.md: a source-turn successor candidate is
/// a retryable minted-ID collision, not a caller transition defect.
#[test]
fn generated_successor_source_candidate_is_a_retryable_collision() {
    let source = TurnId::from_uuid(Uuid::from_u128(1));
    let mut proposed = BTreeSet::new();

    assert!(matches!(
        record_reclassified_turn_candidate(source, source, &mut proposed),
        Err(ModelCallRepositoryError::IdentityCollision(
            ModelCallIdentityCollision::ReclassifiedTurn
        ))
    ));
}

/// docs/spec/model-call-execution.md: a duplicate successor candidate is a
/// retryable minted-ID collision, not a caller transition defect.
#[test]
fn generated_successor_duplicate_is_a_retryable_collision() {
    let source = TurnId::from_uuid(Uuid::from_u128(1));
    let successor = TurnId::from_uuid(Uuid::from_u128(2));
    let mut proposed = BTreeSet::new();

    record_reclassified_turn_candidate(source, successor, &mut proposed)
        .expect("the first source-safe successor is accepted");
    assert!(matches!(
        record_reclassified_turn_candidate(source, successor, &mut proposed),
        Err(ModelCallRepositoryError::IdentityCollision(
            ModelCallIdentityCollision::ReclassifiedTurn
        ))
    ));
}
#[derive(Debug)]
struct ServerCommitFailure {
    code: &'static str,
}

impl fmt::Display for ServerCommitFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("server reported commit failure")
    }
}

impl Error for ServerCommitFailure {}

impl DatabaseError for ServerCommitFailure {
    fn message(&self) -> &str {
        "server reported commit failure"
    }

    fn as_error(&self) -> &(dyn Error + Send + Sync + 'static) {
        self
    }

    fn as_error_mut(&mut self) -> &mut (dyn Error + Send + Sync + 'static) {
        self
    }

    fn into_error(self: Box<Self>) -> Box<dyn Error + Send + Sync + 'static> {
        self
    }

    fn kind(&self) -> ErrorKind {
        ErrorKind::Other
    }

    fn code(&self) -> Option<Cow<'_, str>> {
        Some(Cow::Borrowed(self.code))
    }
}

#[test]
fn lost_commit_response_is_commit_ambiguous() {
    let error = sqlx::Error::Io(io::Error::new(
        io::ErrorKind::ConnectionReset,
        "commit response was lost",
    ));
    let commit_ambiguous = commit_failure_is_ambiguous(&error);

    assert!(commit_ambiguous);
    assert_eq!(
        ModelCallRepositoryError::from_database(error, commit_ambiguous).operator_failure_class(),
        OperatorFailureClass::Infrastructure {
            commit_ambiguous: true
        }
    );
}

#[test]
fn server_rejected_commit_is_not_ambiguous() {
    let error = sqlx::Error::Database(Box::new(ServerCommitFailure { code: "23514" }));
    let commit_ambiguous = commit_failure_is_ambiguous(&error);

    assert!(!commit_ambiguous);
    assert_eq!(
        ModelCallRepositoryError::from_database(error, commit_ambiguous).operator_failure_class(),
        OperatorFailureClass::Infrastructure {
            commit_ambiguous: false
        }
    );
}

#[test]
fn server_reported_unknown_commit_outcomes_are_ambiguous() {
    let transaction_resolution_unknown =
        sqlx::Error::Database(Box::new(ServerCommitFailure { code: "08007" }));
    assert!(commit_failure_is_ambiguous(&transaction_resolution_unknown));

    let statement_completion_unknown =
        sqlx::Error::Database(Box::new(ServerCommitFailure { code: "40003" }));
    assert!(commit_failure_is_ambiguous(&statement_completion_unknown));
}

/// docs/spec/model-call-execution.md: a retained completed observation is
/// present only when the terminal frontier is the exact source prefix,
/// assistant sequence, and final `TurnCompleted` marker.
#[test]
fn completed_reread_requires_exact_terminal_frontier_shape() {
    let session = Uuid::from_u128(1);
    let turn = Uuid::from_u128(2);
    let call = Uuid::from_u128(3);
    let source = vec![(Uuid::from_u128(4), Uuid::from_u128(5))];
    let assistant = vec![signalbox_domain::AssistantResponsePart::Text(
        signalbox_domain::AssistantText::try_new(String::from("exact reply"))
            .expect("fixture text is admitted"),
    )];
    let prefix = StoredTerminalFrontierMember {
        source_session: source[0].0,
        entry: source[0].1,
        payload_kind: String::from("origin_accepted_input"),
        assistant_text: None,
        producing_call: None,
        completed_turn: None,
        failed_turn: None,
        cancelled_turn: None,
    };
    let assistant_member = StoredTerminalFrontierMember {
        source_session: session,
        entry: Uuid::from_u128(6),
        payload_kind: String::from("assistant_text"),
        assistant_text: Some(String::from("exact reply")),
        producing_call: Some(call),
        completed_turn: None,
        failed_turn: None,
        cancelled_turn: None,
    };
    let completion = StoredTerminalFrontierMember {
        source_session: session,
        entry: Uuid::from_u128(7),
        payload_kind: String::from("turn_completed"),
        assistant_text: None,
        producing_call: None,
        completed_turn: Some(turn),
        failed_turn: None,
        cancelled_turn: None,
    };
    let exact = vec![prefix.clone(), assistant_member.clone(), completion.clone()];
    assert!(completed_terminal_frontier_matches(
        &source, &exact, session, turn, call, &assistant,
    ));

    assert!(!completed_terminal_frontier_matches(
        &source,
        &[prefix.clone(), assistant_member.clone()],
        session,
        turn,
        call,
        &assistant,
    ));
    let mut extra = exact.clone();
    extra.insert(1, prefix.clone());
    assert!(!completed_terminal_frontier_matches(
        &source, &extra, session, turn, call, &assistant,
    ));
    let mut wrong_marker = completion;
    wrong_marker.completed_turn = Some(Uuid::from_u128(8));
    assert!(!completed_terminal_frontier_matches(
        &source,
        &[prefix, assistant_member, wrong_marker],
        session,
        turn,
        call,
        &assistant,
    ));
}

/// docs/spec/model-call-execution.md: a retained failed observation is
/// present only when its terminal frontier is the exact source prefix
/// plus one matching failure marker.
#[test]
fn failed_reread_requires_exact_terminal_frontier_shape() {
    let session = Uuid::from_u128(1);
    let turn = Uuid::from_u128(2);
    let source = vec![(Uuid::from_u128(3), Uuid::from_u128(4))];
    let prefix = StoredTerminalFrontierMember {
        source_session: source[0].0,
        entry: source[0].1,
        payload_kind: String::from("origin_accepted_input"),
        assistant_text: None,
        producing_call: None,
        completed_turn: None,
        failed_turn: None,
        cancelled_turn: None,
    };
    let failure = StoredTerminalFrontierMember {
        source_session: session,
        entry: Uuid::from_u128(5),
        payload_kind: String::from("turn_failed"),
        assistant_text: None,
        producing_call: None,
        completed_turn: None,
        failed_turn: Some(turn),
        cancelled_turn: None,
    };
    assert!(failed_terminal_frontier_matches(
        &source,
        &[prefix.clone(), failure.clone()],
        session,
        turn,
    ));

    let mut wrong_failure = failure;
    wrong_failure.failed_turn = Some(Uuid::from_u128(6));
    assert!(!failed_terminal_frontier_matches(
        &source,
        &[prefix.clone(), wrong_failure],
        session,
        turn,
    ));
    assert!(!failed_terminal_frontier_matches(
        &source,
        &[prefix],
        session,
        turn,
    ));
}

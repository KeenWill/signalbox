use signalbox_application::{
    FixtureToolExecutionTransaction, FixtureTransactionFailures, InProcessToolDispatchGate,
    PreparedAttemptApproval, PreparedAttemptIdentities, PreparedAttemptProposal,
    ToolExecutionService, ToolExecutionServiceOutcome, ToolExecutorEvidence,
    UuidV7ToolLoopIdGenerator, prepared_single_attempt_batch,
};
use signalbox_domain::{
    ContextFrontierId, DurableCommandId, ModelCallId, NormalizedToolArguments, SessionId,
    ToolAttemptDispatchCorrelation, ToolAttemptDispatchCorrelationReconstitutionInput,
    ToolAttemptEnd, ToolAttemptId, ToolDispatchGeneration, ToolEffectClass, ToolName,
    ToolRequestId, TurnAttemptId, TurnId,
};
use signalbox_model_runtime::{
    CredentialAccess, CredentialAccessError, CredentialReference, CredentialValue,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use super::{
    diagnostic::*, redaction::*, request::*, result::*, test_provider_support::*, test_support::*,
    test_telemetry_support::*, tool::*, transport::*, transport_failure::*,
};

pub(super) const FIXED_QUERY_PARAMETER_COLLISION_KEY: &str = "text_decorations";

pub(super) const SERIALIZED_URL_SYNTAX_COLLISION_KEY: &str = "?";

pub(super) const REQUEST_DEBUG_COLLISION_KEY: &str = "provider";

pub(super) const CREDENTIAL_DEBUG_COLLISION_KEY: &str = "REDACTED";

pub(super) const REQUEST_CREDENTIAL_DEBUG_COLLISION_KEY: &str = "} CredentialValue";

pub(super) const REMOVED_RESPONSE_DEBUG_FIELD_KEY: &str = "result_count";

pub(super) const SUCCESS_PAYLOAD_DELIMITER_COLLISION_KEY: &str = "[";

pub(super) const SUCCESS_PAYLOAD_EMPTY_RESULTS_COLLISION_KEY: &str = "[]";

pub(super) const SUCCESS_PAYLOAD_MULTI_RESULT_COLLISION_KEY: &str = "},{";

pub(super) const SUCCESS_FIELD_BOUNDARY_COLLISION_KEY: &str = r#"title":"Synthetic"#;

pub(super) const SUCCESS_FIELD_BOUNDARY_COLLISION_VALUE: &str = "Synthetic boundary result";

pub(super) const SUCCESS_FIELD_TRAILING_BOUNDARY_COLLISION_KEY: &str = r#"Synthetic","url"#;

pub(super) const SUCCESS_FIELD_TRAILING_BOUNDARY_COLLISION_VALUE: &str = "Boundary Synthetic";

pub(super) const SUCCESS_SNIPPET_TITLE_BOUNDARY_COLLISION_KEY: &str = r#"Synthetic","title"#;

pub(super) const SUCCESS_SNIPPET_TITLE_BOUNDARY_COLLISION_VALUE: &str = "Boundary Synthetic";

pub(super) const SUCCESS_DYNAMIC_FIELD_BOUNDARY_COLLISION_KEY: &str = r#"AAA","title":"BBB"#;

pub(super) const SUCCESS_DYNAMIC_FIELD_BOUNDARY_TITLE: &str = "BBB boundary title";

pub(super) const SUCCESS_DYNAMIC_FIELD_BOUNDARY_SNIPPET: &str = "boundary snippet AAA";

pub(super) const SUCCESS_DYNAMIC_RESULT_BOUNDARY_COLLISION_KEY: &str = r#"AAA"},{"snippet":"BBB"#;

pub(super) const SUCCESS_DYNAMIC_RESULT_BOUNDARY_URL: &str = "https://example.com/AAA";

pub(super) const SUCCESS_DYNAMIC_RESULT_BOUNDARY_SNIPPET: &str = "BBB boundary snippet";

pub(super) const BOUND_WRAPPER_DYNAMIC_PREFIX_COLLISION_KEY: &str =
    r#"CompletedText("{\"results\":[{\"snippet\":\"Synthetic"#;

pub(super) const BOUND_WRAPPER_DYNAMIC_PREFIX_COLLISION_VALUE: &str = "Synthetic boundary snippet";

pub(super) const RESULT_DEBUG_COLLISION_KEY: &str = "[provider-controlled]";

pub(super) const REQUEST_DETAIL_COLLISION_KEY: &str = "web search request failed";

pub(super) const QUERY_CASE_NORMALIZED_COLLISION_KEY: &str = "ABCDEF";

pub(super) const QUERY_CASE_NORMALIZED_COLLISION_VALUE: &str = "%61%62%63%64%65%66";

pub(super) const SHORT_DIAGNOSTIC_COLLISION_KEY: &str = "r";

pub(super) const TRAILING_HEADER_WHITESPACE_KEY: &[u8] = b"fixture-search-key\t";

pub(super) const EMPTY_CREDENTIAL_VALUE: &[u8] = b"";

pub(super) const NON_UTF8_CREDENTIAL_VALUE: &[u8] = &[0xff];

pub(super) const INTERIOR_NEWLINE_CREDENTIAL_VALUE: &[u8] = b"fixture\nsearch-key";

pub(super) const BOUNDARY_WHITESPACE_BOUND_COLLISION_KEY: &[u8] = b"KnownFailed ";

pub(super) const TIMESTAMP_COLLISION_KEY: &str = "2026";

pub(super) const FORMATTER_EVENT_BOUNDARY_COLLISION_KEY: &str =
    "Z  WARN signalbox_tools_web_web_search";

pub(super) const EXECUTOR_OUTCOME_COLLISION_KEY: &str = "CompletedText";

pub(super) const EXECUTOR_CASE_NORMALIZED_OUTCOME_COLLISION_KEY: &str = "completedtext";

pub(super) const EXECUTOR_KNOWN_FAILURE_TOKEN_COLLISION_KEY: &str = "knownfailed";

pub(super) const EXECUTOR_KNOWN_FAILURE_SUBSTRING_COLLISION_KEY: &str = "known";

pub(super) const EXECUTOR_KNOWN_FAILURE_WORD_COLLISION_KEY: &str = "failed";

pub(super) const EXECUTOR_POPULATED_FAILURE_COLLISION_KEY: &str = "Some";

pub(super) const EXECUTOR_INVALID_RESPONSE_POPULATED_COLLISION_KEY: &str =
    r#"Some(ToolExecutionErrorDetail("web search provider returned"#;

pub(super) const EXECUTOR_PUNCTUATED_OUTCOME_COLLISION_KEY: &str = "completedtext(";

pub(super) const EXECUTOR_ERROR_COLLISION_KEY: &str = "Err";

pub(super) const EXECUTOR_INJECTED_DEBUG_COLLISION_KEY: &str = "[injected]";

pub(super) const EXECUTOR_OK_WRAPPER_COLLISION_KEY: &str = "ok";

pub(super) const EXECUTOR_BOUND_WRAPPER_COLLISION_KEY: &str = "correlated";

pub(super) const EXECUTOR_BOUND_WRAPPER_FIELD_COLLISION_KEY: &str = "{ fence:";

pub(super) const EXECUTOR_POPULATED_SUCCESS_WRAPPER_COLLISION_KEY: &str = "CompletedText(\"{";

pub(super) const CASE_NORMALIZED_REQUEST_DETAIL_COLLISION_KEY: &str = "WEB SEARCH REQUEST FAILED";

pub(super) const DYNAMIC_PROVIDER_REJECTION_WRAPPER_COLLISION_KEY: &str = r#"Some(ToolExecutionErrorDetail("web search provider rejected the request with HTTP status 429"#;

pub(super) const OVERSIZED_CREDENTIAL_TELEMETRY_COLLISION_VALUE: &str =
    "web search credential value was unusable failure=Unusable";

pub(super) const OVERSIZED_BOUND_WRAPPER_COLLISION_VALUE: &str =
    "CorrelatedToolExecutorEvidence { fence: IssuedExecutorFence";

pub(super) const RESPONSE_SANITIZATION_CASE_NORMALIZED_COLLISION_KEY: &str = "evidenceencoding";

pub(super) const SESSION_IDENTITY: u128 = 1;

pub(super) const TURN_IDENTITY: u128 = 2;

pub(super) const ISSUING_ATTEMPT_IDENTITY: u128 = 3;

pub(super) const REQUEST_IDENTITY: u128 = 4;

pub(super) const ATTEMPT_IDENTITY: u128 = 5;

pub(super) const PRODUCING_CALL_IDENTITY: u128 = 6;

pub(super) const FRONTIER_IDENTITY: u128 = 7;

pub(super) const APPROVAL_IDENTITY: u128 = 8;

pub(super) struct CountingCredentials {
    pub(super) resolutions: Arc<AtomicUsize>,
}

impl CredentialAccess for CountingCredentials {
    async fn resolve(
        &self,
        _reference: &CredentialReference,
    ) -> Result<CredentialValue, CredentialAccessError> {
        self.resolutions.fetch_add(1, Ordering::Relaxed);
        Ok(CredentialValue::new(SYNTHETIC_KEY.as_bytes().to_vec()))
    }
}

pub(super) struct CountingTransport {
    pub(super) searches: Arc<AtomicUsize>,
}

impl WebSearchTransport for CountingTransport {
    async fn search(
        &mut self,
        _request: WebSearchRequest,
        credential: &CredentialValue,
    ) -> WebSearchTransportOutcome {
        self.searches.fetch_add(1, Ordering::Relaxed);
        WebSearchTransportOutcome::completed(response_with_result_count(1), credential)
    }
}

pub(super) struct SuccessFieldBoundaryTransport {
    pub(super) searches: Arc<AtomicUsize>,
    pub(super) title: &'static str,
    pub(super) snippet: &'static str,
}

pub(super) struct SuccessResultBoundaryTransport {
    pub(super) searches: Arc<AtomicUsize>,
}

impl WebSearchTransport for SuccessResultBoundaryTransport {
    async fn search(
        &mut self,
        _request: WebSearchRequest,
        credential: &CredentialValue,
    ) -> WebSearchTransportOutcome {
        self.searches.fetch_add(1, Ordering::Relaxed);
        let first = WebSearchResult::try_new(WebSearchResultFields {
            title: String::from(FIXTURE_RESULT_TITLE),
            url: String::from(SUCCESS_DYNAMIC_RESULT_BOUNDARY_URL),
            snippet: String::from(FIXTURE_RESULT_SNIPPET),
        })
        .expect("first result-boundary fixture is admitted");
        let second = WebSearchResult::try_new(WebSearchResultFields {
            title: String::from(FIXTURE_RESULT_TITLE),
            url: String::from(FIXTURE_RESULT_URL),
            snippet: String::from(SUCCESS_DYNAMIC_RESULT_BOUNDARY_SNIPPET),
        })
        .expect("second result-boundary fixture is admitted");
        WebSearchTransportOutcome::completed(
            WebSearchResponse::new(vec![first, second], WebSearchPageCompleteness::Complete)
                .expect("result-boundary fixture response is admitted"),
            credential,
        )
    }
}

impl WebSearchTransport for SuccessFieldBoundaryTransport {
    async fn search(
        &mut self,
        _request: WebSearchRequest,
        credential: &CredentialValue,
    ) -> WebSearchTransportOutcome {
        self.searches.fetch_add(1, Ordering::Relaxed);
        let reflected = WebSearchResult::try_new(WebSearchResultFields {
            title: String::from(self.title),
            url: String::from(FIXTURE_RESULT_URL),
            snippet: String::from(self.snippet),
        })
        .expect("boundary-collision fixture result is admitted");
        WebSearchTransportOutcome::completed(
            WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
                .expect("boundary-collision fixture response is admitted"),
            credential,
        )
    }
}

pub(super) struct ResultDebugFormattingTransport {
    pub(super) searches: Arc<AtomicUsize>,
}

impl WebSearchTransport for ResultDebugFormattingTransport {
    async fn search(
        &mut self,
        _request: WebSearchRequest,
        credential: &CredentialValue,
    ) -> WebSearchTransportOutcome {
        self.searches.fetch_add(1, Ordering::Relaxed);
        let reflected = result(FIXTURE_RESULT_TITLE);
        tracing::warn!(result = ?reflected, "synthetic result diagnostic");
        WebSearchTransportOutcome::completed(
            WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
                .expect("result-debug fixture response is admitted"),
            credential,
        )
    }
}

pub(super) struct StaticCredentials {
    pub(super) value: &'static str,
}

impl CredentialAccess for StaticCredentials {
    async fn resolve(
        &self,
        _reference: &CredentialReference,
    ) -> Result<CredentialValue, CredentialAccessError> {
        Ok(CredentialValue::new(self.value.as_bytes().to_vec()))
    }
}

pub(super) struct RawCredentials {
    pub(super) value: Vec<u8>,
}

impl CredentialAccess for RawCredentials {
    async fn resolve(
        &self,
        _reference: &CredentialReference,
    ) -> Result<CredentialValue, CredentialAccessError> {
        Ok(CredentialValue::new(self.value.clone()))
    }
}

pub(super) struct ProviderStatusCredentials;

impl CredentialAccess for ProviderStatusCredentials {
    async fn resolve(
        &self,
        _reference: &CredentialReference,
    ) -> Result<CredentialValue, CredentialAccessError> {
        Ok(CredentialValue::new(
            PROVIDER_REJECTION_STATUS.to_string().into_bytes(),
        ))
    }
}

pub(super) struct SanitizedDispatchUnknownTransport;

impl WebSearchTransport for SanitizedDispatchUnknownTransport {
    async fn search(
        &mut self,
        _request: WebSearchRequest,
        credential: &CredentialValue,
    ) -> WebSearchTransportOutcome {
        WebSearchTransportOutcome::failed(WebSearchTransportFailure::DispatchUnknown, credential)
    }
}

pub(super) struct RequestFailedTransport {
    pub(super) searches: Arc<AtomicUsize>,
}

impl WebSearchTransport for RequestFailedTransport {
    async fn search(
        &mut self,
        _request: WebSearchRequest,
        credential: &CredentialValue,
    ) -> WebSearchTransportOutcome {
        self.searches.fetch_add(1, Ordering::Relaxed);
        WebSearchTransportOutcome::failed(WebSearchTransportFailure::RequestFailed, credential)
    }
}

pub(super) struct ProviderRejectedTransport {
    pub(super) searches: Arc<AtomicUsize>,
}

pub(super) struct InvalidResponseTransport {
    pub(super) searches: Arc<AtomicUsize>,
}

impl WebSearchTransport for InvalidResponseTransport {
    async fn search(
        &mut self,
        _request: WebSearchRequest,
        credential: &CredentialValue,
    ) -> WebSearchTransportOutcome {
        self.searches.fetch_add(1, Ordering::Relaxed);
        WebSearchTransportOutcome::failed(WebSearchTransportFailure::InvalidResponse, credential)
    }
}

impl WebSearchTransport for ProviderRejectedTransport {
    async fn search(
        &mut self,
        _request: WebSearchRequest,
        credential: &CredentialValue,
    ) -> WebSearchTransportOutcome {
        self.searches.fetch_add(1, Ordering::Relaxed);
        let error = WebSearchProviderError::new(
            PROVIDER_REJECTION_STATUS,
            br#"{"message":"synthetic rejection"}"#.to_vec(),
        )
        .expect("fixture provider error is admitted");
        WebSearchTransportOutcome::failed(
            WebSearchTransportFailure::ProviderRejected(error),
            credential,
        )
    }
}

pub(super) struct ReflectedTitleTransport;

impl WebSearchTransport for ReflectedTitleTransport {
    async fn search(
        &mut self,
        _request: WebSearchRequest,
        credential: &CredentialValue,
    ) -> WebSearchTransportOutcome {
        let title = String::from_utf8(credential.expose_bytes().to_vec())
            .expect("fixture credential is UTF-8");
        let reflected = WebSearchResult::try_new(WebSearchResultFields {
            title,
            url: String::from(FIXTURE_RESULT_URL),
            snippet: String::from(FIXTURE_RESULT_SNIPPET),
        })
        .expect("fixture reflected result is admitted");
        WebSearchTransportOutcome::completed(
            WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
                .expect("fixture response is admitted"),
            credential,
        )
    }
}

/// Serves this crate's prepared batch, failing in `web_search`'s own error type.
///
/// The transaction body itself is shared: every provider-adapter crate needs
/// the same one-prepared-attempt behaviour, so it lives in
/// `signalbox_application`'s `test-support` feature rather than being
/// reimplemented beside each provider. What is `web_search`'s own, and stays
/// here, is which of its error variants each failure surfaces as.
pub(super) fn executor_fixture_transaction(
    batch: signalbox_domain::ToolBatch,
) -> FixtureToolExecutionTransaction<WebSearchExecutorError> {
    FixtureToolExecutionTransaction::new(
        batch,
        FixtureTransactionFailures {
            domain_rejection: WebSearchExecutorError::ArgumentValidationDrift,
            declined_crash_classification: WebSearchExecutorError::EvidenceEncoding,
        },
    )
}

pub(super) fn prepared_web_search_batch() -> signalbox_domain::ToolBatch {
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
            name: ToolName::try_new(String::from(WEB_SEARCH_NAME)).expect("fixture name is valid"),
            arguments: arguments(&serde_json::json!({"query": FIXTURE_QUERY}).to_string()),
            effect_class: ToolEffectClass::ExternalEffect,
            // `web_search` is declared `ToolPermissionDefault::Confirm`, so a
            // policy approval would describe a batch the application never
            // prepares for it.
            approval: PreparedAttemptApproval::UserConfirmation {
                command: DurableCommandId::from_uuid(uuid::Uuid::from_u128(APPROVAL_IDENTITY)),
            },
        },
    )
}

pub(super) async fn execute_raw_credential_through_service(
    value: &[u8],
) -> (ToolExecutionServiceOutcome, usize) {
    let searches = Arc::new(AtomicUsize::new(0));
    let credentials = RawCredentials {
        value: value.to_vec(),
    };
    let transport = CountingTransport {
        searches: Arc::clone(&searches),
    };
    let (catalog, executor) = WebSearchTool::try_new(credentials, transport, configuration())
        .expect("fixture web_search tool compiles")
        .into_parts();
    let batch = prepared_web_search_batch();
    let mut service = ToolExecutionService::new(
        UuidV7ToolLoopIdGenerator,
        executor_fixture_transaction(batch.clone()),
        catalog,
        executor,
        InProcessToolDispatchGate::default(),
    );
    let outcome = service
        .execute(batch.session(), batch.turn())
        .await
        .expect("invalid credential commits definitive evidence");
    (outcome, searches.load(Ordering::Relaxed))
}

pub(super) async fn execute_formatted_raw_credential_through_service(
    value: &[u8],
) -> (bool, usize, String) {
    let diagnostic = Arc::new(Mutex::new(String::new()));
    let searches = Arc::new(AtomicUsize::new(0));
    let credentials = RawCredentials {
        value: value.to_vec(),
    };
    let transport = CountingTransport {
        searches: Arc::clone(&searches),
    };
    let (catalog, executor) = WebSearchTool::try_new(credentials, transport, configuration())
        .expect("fixture web_search tool compiles")
        .into_parts();
    let executor = FormattingExecutor {
        inner: executor,
        diagnostic: Arc::clone(&diagnostic),
    };
    let batch = prepared_web_search_batch();
    let mut service = ToolExecutionService::new(
        UuidV7ToolLoopIdGenerator,
        executor_fixture_transaction(batch.clone()),
        catalog,
        executor,
        InProcessToolDispatchGate::default(),
    );

    let outcome = service.execute(batch.session(), batch.turn()).await;
    let rendered = diagnostic
        .lock()
        .expect("captured executor diagnostic lock is available")
        .clone();
    (outcome.is_err(), searches.load(Ordering::Relaxed), rendered)
}

pub(super) async fn execute_request_failure_through_service(
    value: &'static str,
) -> (ToolExecutionServiceOutcome, usize) {
    let searches = Arc::new(AtomicUsize::new(0));
    let credentials = StaticCredentials { value };
    let transport = RequestFailedTransport {
        searches: Arc::clone(&searches),
    };
    let (catalog, executor) = WebSearchTool::try_new(credentials, transport, configuration())
        .expect("fixture web_search tool compiles")
        .into_parts();
    let batch = prepared_web_search_batch();
    let mut service = ToolExecutionService::new(
        UuidV7ToolLoopIdGenerator,
        executor_fixture_transaction(batch.clone()),
        catalog,
        executor,
        InProcessToolDispatchGate::default(),
    );
    let outcome = service
        .execute(batch.session(), batch.turn())
        .await
        .expect("request failure commits definitive evidence");
    (outcome, searches.load(Ordering::Relaxed))
}

pub(super) async fn execute_provider_rejection_through_service<Credentials>(
    credentials: Credentials,
) -> (ToolExecutionServiceOutcome, usize)
where
    Credentials: CredentialAccess,
{
    let searches = Arc::new(AtomicUsize::new(0));
    let transport = ProviderRejectedTransport {
        searches: Arc::clone(&searches),
    };
    let (catalog, executor) = WebSearchTool::try_new(credentials, transport, configuration())
        .expect("fixture web_search tool compiles")
        .into_parts();
    let batch = prepared_web_search_batch();
    let mut service = ToolExecutionService::new(
        UuidV7ToolLoopIdGenerator,
        executor_fixture_transaction(batch.clone()),
        catalog,
        executor,
        InProcessToolDispatchGate::default(),
    );
    let outcome = service
        .execute(batch.session(), batch.turn())
        .await
        .expect("provider rejection commits definitive evidence");
    (outcome, searches.load(Ordering::Relaxed))
}

pub(super) fn is_committed_known_failure(outcome: &ToolExecutionServiceOutcome) -> bool {
    match committed_tool_attempt_end(outcome) {
        Some(ToolAttemptEnd::KnownFailed { .. }) => true,
        Some(
            ToolAttemptEnd::Completed { .. }
            | ToolAttemptEnd::AwaitingChild { .. }
            | ToolAttemptEnd::Ambiguous,
        )
        | None => false,
    }
}

pub(super) fn is_committed_completed(outcome: &ToolExecutionServiceOutcome) -> bool {
    match committed_tool_attempt_end(outcome) {
        Some(ToolAttemptEnd::Completed { .. }) => true,
        Some(
            ToolAttemptEnd::KnownFailed { .. }
            | ToolAttemptEnd::AwaitingChild { .. }
            | ToolAttemptEnd::Ambiguous,
        )
        | None => false,
    }
}

pub(super) fn is_committed_known_failure_without_detail(
    outcome: &ToolExecutionServiceOutcome,
) -> bool {
    match committed_tool_attempt_end(outcome) {
        Some(ToolAttemptEnd::KnownFailed { error }) => error.detail().is_none(),
        Some(
            ToolAttemptEnd::Completed { .. }
            | ToolAttemptEnd::AwaitingChild { .. }
            | ToolAttemptEnd::Ambiguous,
        )
        | None => false,
    }
}

pub(super) fn committed_tool_attempt_end(
    outcome: &ToolExecutionServiceOutcome,
) -> Option<&ToolAttemptEnd> {
    match outcome {
        ToolExecutionServiceOutcome::ObservationCommitted(ended) => Some(ended.end()),
        ToolExecutionServiceOutcome::NoWork
        | ToolExecutionServiceOutcome::AwaitingApproval(_)
        | ToolExecutionServiceOutcome::AwaitingRecovery(_)
        | ToolExecutionServiceOutcome::AttemptCheckpointed(_)
        | ToolExecutionServiceOutcome::RunnerOfferCommitted(_)
        | ToolExecutionServiceOutcome::RunnerExecutionPending(_)
        | ToolExecutionServiceOutcome::PreflightFailed(_)
        | ToolExecutionServiceOutcome::ObservationAlreadyCommitted(_)
        | ToolExecutionServiceOutcome::CrashClassified(_)
        | ToolExecutionServiceOutcome::ChildWaitParked(_)
        | ToolExecutionServiceOutcome::ChildWaitResumed(_)
        | ToolExecutionServiceOutcome::ContinuationCheckpointed(_)
        | ToolExecutionServiceOutcome::ContinuationTargetUnavailable(_) => None,
    }
}

pub(super) fn arguments(value: &str) -> NormalizedToolArguments {
    NormalizedToolArguments::try_from_provider_text(value.to_owned())
        .expect("fixture arguments are admitted")
}

pub(super) fn dispatch_correlation() -> ToolAttemptDispatchCorrelation {
    ToolAttemptDispatchCorrelation::reconstitute(
        ToolAttemptDispatchCorrelationReconstitutionInput {
            session: SessionId::from_uuid(uuid::Uuid::from_u128(SESSION_IDENTITY)),
            turn: TurnId::from_uuid(uuid::Uuid::from_u128(TURN_IDENTITY)),
            issuing_attempt: TurnAttemptId::from_uuid(uuid::Uuid::from_u128(
                ISSUING_ATTEMPT_IDENTITY,
            )),
            request: ToolRequestId::from_uuid(uuid::Uuid::from_u128(REQUEST_IDENTITY)),
            attempt: ToolAttemptId::from_uuid(uuid::Uuid::from_u128(ATTEMPT_IDENTITY)),
            generation: ToolDispatchGeneration::first(),
        },
    )
}

pub(super) fn known_failure_detail(evidence: ToolExecutorEvidence) -> Option<String> {
    match evidence {
        ToolExecutorEvidence::KnownFailed { detail } => {
            detail.map(|detail| String::from(detail.as_str()))
        }
        other @ (ToolExecutorEvidence::CompletedText(_) | ToolExecutorEvidence::Ambiguous) => {
            panic!("expected known failure, got {other:?}")
        }
    }
}

pub(super) fn colliding_failure_detail(
    credential: &'static str,
    failure_class: WebSearchTransportFailureClass,
) -> String {
    let credentials = StaticCredentials {
        value: SYNTHETIC_KEY,
    };
    let transport = CountingTransport {
        searches: Arc::new(AtomicUsize::new(0)),
    };
    let (_catalog, executor) = WebSearchTool::try_new(credentials, transport, configuration())
        .expect("fixture web_search tool compiles")
        .into_parts();
    let credential = CredentialValue::new(credential.as_bytes().to_vec());
    let scrubber = CredentialScrubber::try_new(&credential).expect("fixture credential is usable");
    let diagnostic = WebSearchCredentialDiagnostic {
        rendered: String::from("!"),
        failure_class: WebSearchCredentialDiagnosticClass::CallerOrHubBug,
        transport_failure_class: Some(failure_class),
    };
    let evidence = executor
        .credential_diagnostic_evidence(diagnostic, &scrubber)
        .expect("non-ambiguous failure becomes evidence");
    match evidence {
        ToolExecutorEvidence::KnownFailed {
            detail: Some(detail),
        } => String::from(detail.as_str()),
        other @ (ToolExecutorEvidence::KnownFailed { detail: None }
        | ToolExecutorEvidence::CompletedText(_)
        | ToolExecutorEvidence::Ambiguous) => {
            panic!("expected detailed known failure, got {other:?}")
        }
    }
}

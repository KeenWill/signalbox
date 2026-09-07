//! Web evaluation fixtures and verification.

use crate::*;

pub(crate) const WEB_ORIGIN: &str = "https://example.com";
pub(crate) const WEB_URL: &str = "https://example.com/eval";
pub(crate) const WEB_QUERY: &str = "Signalbox tool evaluation";
pub(crate) const WEB_FETCH_BODY: &str = "Signalbox tool evaluation fixture";
pub(crate) const WEB_SEARCH_TITLE: &str = "Synthetic Signalbox result";
pub(crate) const WEB_SEARCH_SNIPPET: &str = "Synthetic result for model-in-the-loop evaluation.";
pub(crate) const EXPECTED_WEB_CREDENTIAL_REFERENCE: &str = "brave-search-primary";
pub(crate) const SYNTHETIC_WEB_CREDENTIAL: &[u8] = b"synthetic-web-eval-key";
pub(crate) const WEB_CASES: &[ForcedCase] = &[
    ForcedCase {
        name: WEB_FETCH_NAME,
        expected_arguments: r#"{"url":"https://example.com/eval"}"#,
        prompt: "Call web_fetch with exactly {\"url\":\"https://example.com/eval\"}. After its result, answer done without another tool call.",
    },
    ForcedCase {
        name: WEB_SEARCH_NAME,
        expected_arguments: r#"{"query":"Signalbox tool evaluation"}"#,
        prompt: "Call web_search with exactly {\"query\":\"Signalbox tool evaluation\"}. After its result, answer done without another tool call.",
    },
];

impl FamilySuite {
    pub(crate) fn web() -> EvalResult<Self> {
        let workspace = tempfile::tempdir()?;
        let fetch = WebFetchTool::try_new(
            FixtureWebFetchTransport,
            WebFetchEgressPolicy::try_from_allowed_origins([String::from(WEB_ORIGIN)])?,
        )?;
        let search = WebSearchTool::try_new(
            FixtureWebCredential,
            FixtureWebSearchTransport,
            WebSearchConfiguration::new(WebSearchProvider::Brave),
        )?;
        let (fetch_catalog, fetch_executor) = fetch.into_parts();
        let (search_catalog, search_executor) = search.into_parts();
        Ok(Self {
            family: EvalFamily::Web,
            workspace,
            git_seed: None,
            git_seed_refs: BTreeMap::new(),
            git_seed_fixture: GitFixtureSnapshot::default(),
            catalog: MergedCatalog::try_new([fetch_catalog, search_catalog])?,
            executor: SharedFamilyExecutor::new(FamilyExecutor::Web {
                fetch: fetch_executor,
                search: search_executor,
            }),
            workspace_seed_entries: BTreeMap::new(),
            workspace_seed_modified_times: BTreeMap::new(),
            workspace_seed_entry_identities: BTreeMap::new(),
            workspace_seed_extended_attributes: BTreeMap::new(),
            workspace_seed_inode_flags: BTreeMap::new(),
            git_pre_execution_worktree_entries: StdMutex::new(None),
            git_pre_execution_worktree_modified_times: StdMutex::new(None),
            git_pre_execution_worktree_entry_identities: StdMutex::new(None),
            git_pre_execution_worktree_extended_attributes: StdMutex::new(None),
            git_pre_execution_metadata_extended_attributes: StdMutex::new(None),
            git_pre_execution_index_entries: StdMutex::new(None),
            git_pre_execution_metadata_root_modified_time: StdMutex::new(None),
            git_pre_execution_metadata_root_identity: StdMutex::new(None),
            git_pre_execution_metadata_top_level: StdMutex::new(None),
            git_pre_execution_objects: StdMutex::new(None),
            git_pre_execution_object_entries: Arc::new(StdMutex::new(None)),
            git_pre_execution_object_modified_times: Arc::new(StdMutex::new(None)),
            git_pre_execution_object_entry_identities: Arc::new(StdMutex::new(None)),
        })
    }
}

pub(crate) fn web_forced_case_passed(
    name: &str,
    arguments: &serde_json::Value,
    result: &serde_json::Value,
) -> bool {
    match name {
        WEB_FETCH_NAME => {
            json_object_has_exact_fields(
                result,
                &[
                    "url",
                    "status",
                    "content_type",
                    "body",
                    "truncated",
                    EVAL_RECEIPT_FIELD,
                ],
            ) && result["url"] == arguments["url"]
                && result["status"] == 200
                && result["content_type"] == "text/plain"
                && result["body"] == WEB_FETCH_BODY
                && result["truncated"] == true
        }
        WEB_SEARCH_NAME => {
            json_object_has_exact_fields(result, &["results", "truncated", EVAL_RECEIPT_FIELD])
                && result["results"].as_array().is_some_and(|results| {
                    results.len() == 1
                        && results.first().is_some_and(|first| {
                            json_object_has_exact_fields(first, &["title", "url", "snippet"])
                                && first["title"] == WEB_SEARCH_TITLE
                                && first["url"] == WEB_URL
                                && first["snippet"] == WEB_SEARCH_SNIPPET
                        })
                })
                && result["truncated"] == true
        }
        _ => false,
    }
}

pub(crate) fn web_natural_result_payloads_passed(
    snapshot: &CaseSnapshot,
    tracker: &OperationTracker,
) -> bool {
    let Ok(Some((search, fetch))) = snapshot.web_natural_request_pair() else {
        return false;
    };
    let Some(search_content) = tracker.result_content(search.request_id) else {
        return false;
    };
    let Some(fetch_content) = tracker.result_content(fetch.request_id) else {
        return false;
    };
    let Ok(search_result) = serde_json::from_str::<serde_json::Value>(&search_content) else {
        return false;
    };
    let Ok(fetch_result) = serde_json::from_str::<serde_json::Value>(&fetch_content) else {
        return false;
    };
    web_forced_case_passed(
        WEB_SEARCH_NAME,
        &serde_json::json!({"query": WEB_QUERY}),
        &search_result,
    ) && web_forced_case_passed(
        WEB_FETCH_NAME,
        &serde_json::json!({"url": WEB_URL}),
        &fetch_result,
    )
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct FixtureWebFetchTransport;

impl WebFetchTransport for FixtureWebFetchTransport {
    async fn fetch(
        &mut self,
        request: WebFetchRequest,
    ) -> Result<WebFetchResponse, WebFetchTransportFailure> {
        if request.url().as_str() != WEB_URL {
            return Err(WebFetchTransportFailure::RequestFailed);
        }
        WebFetchResponse::new(
            200,
            Some(String::from("text/plain")),
            WEB_FETCH_BODY.as_bytes().to_vec(),
            WebFetchBodyCompleteness::Truncated,
        )
        .ok_or(WebFetchTransportFailure::DispatchUnknown)
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct FixtureWebCredential;

impl CredentialAccess for FixtureWebCredential {
    async fn resolve(
        &self,
        reference: &CredentialReference,
    ) -> Result<CredentialValue, CredentialAccessError> {
        assert_eq!(reference.as_str(), EXPECTED_WEB_CREDENTIAL_REFERENCE);
        Ok(CredentialValue::new(SYNTHETIC_WEB_CREDENTIAL.to_vec()))
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct FixtureWebSearchTransport;

impl WebSearchTransport for FixtureWebSearchTransport {
    async fn search(
        &mut self,
        request: WebSearchRequest,
        credential: &CredentialValue,
    ) -> WebSearchTransportOutcome {
        if request.query() != WEB_QUERY || credential.expose_bytes() != SYNTHETIC_WEB_CREDENTIAL {
            return WebSearchTransportOutcome::failed(
                WebSearchTransportFailure::RequestFailed,
                credential,
            );
        }
        let result = WebSearchResult::try_new(WebSearchResultFields {
            title: String::from(WEB_SEARCH_TITLE),
            url: String::from(WEB_URL),
            snippet: String::from(WEB_SEARCH_SNIPPET),
        })
        .expect("the synthetic web result is valid");
        let response =
            WebSearchResponse::new(vec![result], WebSearchPageCompleteness::MoreAvailable)
                .expect("the synthetic web response is bounded");
        WebSearchTransportOutcome::completed(response, credential)
    }
}

#[test]
fn final_response_report_rejects_a_negative_web_report() {
    let tracker = OperationTracker::default();
    let response = format!("I did not find the {WEB_FETCH_BODY}.");
    tracker.observe_response_text(&response, false);

    assert!(!tracker.final_response_reports(WEB_FETCH_BODY));
}

#[test]
fn final_response_report_rejects_a_negated_web_fetch_action() {
    let tracker = OperationTracker::default();
    let response = format!("I could not fetch {WEB_FETCH_BODY}.");
    tracker.observe_response_text(&response, false);

    assert!(!tracker.final_response_reports(WEB_FETCH_BODY));
}

#[test]
fn final_response_report_rejects_a_no_result_web_report() {
    let tracker = OperationTracker::default();
    let response = format!("No result was found for {WEB_FETCH_BODY}.");
    tracker.observe_response_text(&response, false);

    assert!(!tracker.final_response_reports(WEB_FETCH_BODY));
}

#[test]
fn unforced_web_tier_reports_infrastructure_for_an_exact_known_failed_attempt() {
    let outcome = CaseOutcome {
        target: None,
        expected_arguments: None,
        execution_completed: true,
        forced_verification_failed: false,
        tool_results: vec![TrackedToolResult {
            request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
            content: String::from("fixture result"),
            is_error: true,
            round_tripped: true,
        }],
        snapshot: CaseSnapshot {
            turn_disposition: SnapshotTurnDisposition::Completed,
            requests: vec![RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(WEB_SEARCH_NAME),
                arguments_text: serde_json::json!({"query": WEB_QUERY}).to_string(),
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: None,
                attempt_succeeded: false,
                attempt_denied: false,
            }],
            model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
        },
    };

    assert_eq!(
        outcome.natural_loop_disposition(EvalFamily::Web),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn unforced_web_tier_keeps_a_schema_invalid_extra_field_as_a_miss() {
    let snapshot = failed_request_snapshot(
        WEB_FETCH_NAME,
        serde_json::json!({"url": WEB_URL, "unexpected": true}),
    );

    assert!(!snapshot.exact_natural_request_failed(EvalFamily::Web));
}

#[test]
fn forced_web_search_verifier_rejects_an_extra_result() -> EvalResult {
    let suite = FamilySuite::web()?;
    let case = WEB_CASES
        .iter()
        .find(|case| case.name == WEB_SEARCH_NAME)
        .expect("the web search fixture exists");
    let fixture = serde_json::json!({
        "title": WEB_SEARCH_TITLE,
        "url": WEB_URL,
        "snippet": WEB_SEARCH_SNIPPET,
    });
    let result = serde_json::json!({
        "results": [fixture.clone(), fixture],
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_web_search_verifier_rejects_an_unexpected_result_field() -> EvalResult {
    let suite = FamilySuite::web()?;
    let case = WEB_CASES
        .iter()
        .find(|case| case.name == WEB_SEARCH_NAME)
        .expect("the web search fixture exists");
    let result = serde_json::json!({
        "results": [{
            "title": WEB_SEARCH_TITLE,
            "url": WEB_URL,
            "snippet": WEB_SEARCH_SNIPPET,
            "error": "synthetic contradictory field",
        }],
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_web_search_verifier_accepts_distinct_incomplete_evidence() -> EvalResult {
    let suite = FamilySuite::web()?;
    let case = WEB_CASES
        .iter()
        .find(|case| case.name == WEB_SEARCH_NAME)
        .expect("the web search fixture exists");
    let result = serde_json::json!({
        "results": [{
            "title": WEB_SEARCH_TITLE,
            "url": WEB_URL,
            "snippet": WEB_SEARCH_SNIPPET,
        }],
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_web_fetch_verifier_accepts_truncated_body_evidence() -> EvalResult {
    let suite = FamilySuite::web()?;
    let case = WEB_CASES
        .iter()
        .find(|case| case.name == WEB_FETCH_NAME)
        .expect("the web fetch fixture exists");
    let result = serde_json::json!({
        "url": WEB_URL,
        "status": 200,
        "content_type": "text/plain",
        "body": WEB_FETCH_BODY,
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_web_fetch_verifier_rejects_an_unexpected_result_field() -> EvalResult {
    let suite = FamilySuite::web()?;
    let case = WEB_CASES
        .iter()
        .find(|case| case.name == WEB_FETCH_NAME)
        .expect("the web fetch fixture exists");
    let result = serde_json::json!({
        "url": WEB_URL,
        "status": 200,
        "content_type": "text/plain",
        "body": WEB_FETCH_BODY,
        "truncated": true,
        "error": "synthetic contradictory field",
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn web_natural_state_requires_a_later_model_call_for_the_fetch() -> EvalResult {
    let snapshot = CaseSnapshot {
        turn_disposition: SnapshotTurnDisposition::Completed,
        requests: vec![
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(WEB_SEARCH_NAME),
                arguments_text: normalized_arguments_text(
                    &serde_json::json!({"query": WEB_QUERY}).to_string(),
                )?,
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
                attempt_denied: false,
            },
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(WEB_FETCH_NAME),
                arguments_text: normalized_arguments_text(
                    &serde_json::json!({"url": WEB_URL}).to_string(),
                )?,
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
                attempt_denied: false,
            },
        ],
        model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
    };

    assert!(!snapshot.web_natural_requests_passed()?);
    Ok(())
}

#[test]
fn web_natural_state_requires_the_search_result_before_the_fetch_call() -> EvalResult {
    let snapshot = CaseSnapshot {
        turn_disposition: SnapshotTurnDisposition::Completed,
        requests: vec![
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(WEB_SEARCH_NAME),
                arguments_text: normalized_arguments_text(
                    &serde_json::json!({"query": WEB_QUERY}).to_string(),
                )?,
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_LATE_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
                attempt_denied: false,
            },
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_MODEL_CALL_ID),
                name: String::from(WEB_FETCH_NAME),
                arguments_text: normalized_arguments_text(
                    &serde_json::json!({"url": WEB_URL}).to_string(),
                )?,
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
                attempt_denied: false,
            },
        ],
        model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
    };

    assert!(!snapshot.web_natural_requests_passed()?);
    Ok(())
}

#[test]
fn web_natural_state_requires_the_exact_query() -> EvalResult {
    let snapshot = CaseSnapshot {
        turn_disposition: SnapshotTurnDisposition::Completed,
        requests: vec![
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(WEB_SEARCH_NAME),
                arguments_text: String::from(r#"{"query":"different query"}"#),
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
                attempt_denied: false,
            },
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_MODEL_CALL_ID),
                name: String::from(WEB_FETCH_NAME),
                arguments_text: serde_json::json!({"url": WEB_URL}).to_string(),
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
                attempt_denied: false,
            },
        ],
        model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
    };

    assert!(!snapshot.web_natural_requests_passed()?);
    Ok(())
}

#[test]
fn web_natural_state_accepts_a_valid_pair_after_a_premature_fetch() -> EvalResult {
    let snapshot = CaseSnapshot {
        turn_disposition: SnapshotTurnDisposition::Completed,
        requests: vec![
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(WEB_FETCH_NAME),
                arguments_text: normalized_arguments_text(
                    &serde_json::json!({"url": WEB_URL}).to_string(),
                )?,
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
                attempt_denied: false,
            },
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(WEB_SEARCH_NAME),
                arguments_text: normalized_arguments_text(
                    &serde_json::json!({"query": WEB_QUERY}).to_string(),
                )?,
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
                attempt_denied: false,
            },
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_MODEL_CALL_ID),
                name: String::from(WEB_FETCH_NAME),
                arguments_text: normalized_arguments_text(
                    &serde_json::json!({"url": WEB_URL}).to_string(),
                )?,
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
                attempt_denied: false,
            },
        ],
        model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
    };

    assert!(snapshot.web_natural_requests_passed()?);
    Ok(())
}

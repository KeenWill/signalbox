use std::{
    error::Error,
    fmt,
    future::{Future, ready},
    pin::pin,
    task::{Context, Poll, Waker},
};

use serde_json::{Value, json};
use signalbox_application::{ToolCatalog, ToolCatalogValidationFailure};
use signalbox_domain::{
    ToolAttemptDispatchCorrelationReconstitutionInput, ToolAttemptId, ToolDispatchGeneration,
    ToolName, ToolRequestId, TurnAttemptId, TurnId,
};

use super::*;

const INITIAL_TEXT: &str = "Implement the durable plan";
const REVISED_TEXT: &str = "Implement and validate the durable plan";
const SECOND_TEXT: &str = "Open the pull request";

fn correlation(session_seed: u128) -> ToolAttemptDispatchCorrelation {
    ToolAttemptDispatchCorrelation::reconstitute(
        ToolAttemptDispatchCorrelationReconstitutionInput {
            session: SessionId::from_uuid(uuid::Uuid::from_u128(session_seed)),
            turn: TurnId::from_uuid(uuid::Uuid::from_u128(session_seed + 1)),
            issuing_attempt: TurnAttemptId::from_uuid(uuid::Uuid::from_u128(session_seed + 2)),
            request: ToolRequestId::from_uuid(uuid::Uuid::from_u128(session_seed + 3)),
            attempt: ToolAttemptId::from_uuid(uuid::Uuid::from_u128(session_seed + 4)),
            generation: ToolDispatchGeneration::first(),
        },
    )
}

fn provenance_with_attempt(
    provenance: PlanEventProvenance,
    attempt_seed: u128,
) -> PlanEventProvenance {
    let base = provenance.correlation();
    PlanEventProvenance::from_invocation(ToolAttemptDispatchCorrelation::reconstitute(
        ToolAttemptDispatchCorrelationReconstitutionInput {
            session: base.session(),
            turn: base.turn(),
            issuing_attempt: base.issuing_attempt(),
            request: base.request(),
            attempt: ToolAttemptId::from_uuid(uuid::Uuid::from_u128(attempt_seed)),
            generation: base.generation(),
        },
    ))
}

fn ordinal(value: u64) -> PlanEventOrdinal {
    PlanEventOrdinal::try_from_u64(value).expect("fixture event ordinal is positive")
}

fn entry(value: u64) -> PlanEntryId {
    PlanEntryId::try_from_u64(value).expect("fixture entry identity is positive")
}

fn text(value: &str) -> PlanText {
    PlanText::try_new(value.to_owned()).expect("fixture plan text is bounded")
}

fn event(value: u64, provenance: PlanEventProvenance, kind: PlanEventKind) -> PlanEvent {
    PlanEvent::new(ordinal(value), provenance, kind)
}

fn arguments(value: Value) -> NormalizedToolArguments {
    NormalizedToolArguments::try_from_provider_text(value.to_string())
        .expect("fixture arguments are admitted")
}

fn run_ready<Output>(future: impl Future<Output = Output>) -> Output {
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);
    let mut future = pin!(future);
    match future.as_mut().poll(&mut context) {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("fake-backed plan execution must be immediately ready"),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FakeError;

impl fmt::Display for FakeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("fake plan port failed")
    }
}

impl Error for FakeError {}

impl ClassifyOperatorFailure for FakeError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        OperatorFailureClass::Infrastructure {
            commit_ambiguous: false,
        }
    }
}

#[derive(Debug)]
struct FakePort {
    append_result: Option<PlanAppendOutcome>,
    read_result: Option<PlanReadPage>,
    append_requests: Vec<PlanAppendRequest>,
    read_requests: Vec<PlanReadRequest>,
}

impl FakePort {
    fn appending(result: PlanEvent) -> Self {
        Self {
            append_result: Some(PlanAppendOutcome::Appended(result)),
            read_result: None,
            append_requests: Vec::new(),
            read_requests: Vec::new(),
        }
    }

    fn rejecting(result: PlanAppendRejection) -> Self {
        Self {
            append_result: Some(PlanAppendOutcome::Rejected(result)),
            read_result: None,
            append_requests: Vec::new(),
            read_requests: Vec::new(),
        }
    }

    fn reading(result: PlanReadPage) -> Self {
        Self {
            append_result: None,
            read_result: Some(result),
            append_requests: Vec::new(),
            read_requests: Vec::new(),
        }
    }
}

impl SessionPlanPort for FakePort {
    type Error = FakeError;

    fn append_plan_event(
        &mut self,
        request: PlanAppendRequest,
    ) -> impl Future<Output = Result<PlanAppendOutcome, Self::Error>> + Send {
        self.append_requests.push(request);
        ready(Ok(self
            .append_result
            .take()
            .expect("fixture append result was configured")))
    }

    fn read_plan(
        &mut self,
        request: PlanReadRequest,
    ) -> impl Future<Output = Result<PlanReadPage, Self::Error>> + Send {
        self.read_requests.push(request);
        ready(Ok(self
            .read_result
            .take()
            .expect("fixture read result was configured")))
    }
}

fn catalog() -> CompiledToolCatalog {
    let provenance = PlanEventProvenance::from_invocation(correlation(10));
    let stored = event(
        1,
        provenance,
        PlanEventKind::Created {
            text: text(INITIAL_TEXT),
        },
    );
    PlanTools::try_new(FakePort::appending(stored))
        .expect("static plan tools compile")
        .into_parts()
        .0
}

fn completed_text(evidence: ToolExecutorEvidence) -> String {
    match evidence {
        ToolExecutorEvidence::CompletedText(result) => result,
        ToolExecutorEvidence::KnownFailed { .. } => {
            panic!("fixture execution unexpectedly returned a known failure")
        }
        ToolExecutorEvidence::Ambiguous => {
            panic!("fixture execution unexpectedly returned ambiguous evidence")
        }
    }
}

fn only_entry(plan: &FoldedPlan) -> &PlanEntry {
    let [current] = plan.entries() else {
        panic!("one created entry remains visible")
    };
    current
}

fn ordered_pair(plan: &FoldedPlan) -> (&PlanEntry, &PlanEntry) {
    let [first, later] = plan.entries() else {
        panic!("two entries remain in creation order")
    };
    (first, later)
}

fn only_append_request(port: &FakePort) -> &PlanAppendRequest {
    let [observed] = port.append_requests.as_slice() else {
        panic!("one append request is observed")
    };
    observed
}

fn only_read_request(port: &FakePort) -> &PlanReadRequest {
    let [observed] = port.read_requests.as_slice() else {
        panic!("one read request is observed")
    };
    observed
}

fn is_port_contract<PortError>(error: &PlanExecutorError<PortError>) -> bool {
    match error {
        PlanExecutorError::PortContract => true,
        PlanExecutorError::ArgumentValidationDrift
        | PlanExecutorError::Port(_)
        | PlanExecutorError::ResultEncoding => false,
    }
}

fn known_failure_has_detail(evidence: &ToolExecutorEvidence) -> bool {
    match evidence {
        ToolExecutorEvidence::KnownFailed { detail: Some(_) } => true,
        ToolExecutorEvidence::CompletedText(_)
        | ToolExecutorEvidence::KnownFailed { detail: None }
        | ToolExecutorEvidence::Ambiguous => false,
    }
}

fn oversized_read_page(provenance: PlanEventProvenance) -> PlanReadPage {
    let large_text = text(&"🦀".repeat(MAX_PLAN_TEXT_CHARS));
    let entries = (1..=MAX_PLAN_READ_ENTRIES)
        .map(|value| PlanEntry::new(entry(value as u64), large_text.clone(), PlanStatus::Pending))
        .collect();
    let events = (1..=MAX_PLAN_HISTORY_EVENTS)
        .map(|value| {
            event(
                value as u64,
                provenance_with_attempt(provenance, 1_000 + value as u128),
                PlanEventKind::Created {
                    text: large_text.clone(),
                },
            )
        })
        .collect();
    PlanReadPage::new(
        provenance.session(),
        entries,
        PlanPageCompleteness::Complete,
        Some(PlanHistoryPage::new(events, PlanPageCompleteness::Complete)),
    )
}

#[test]
fn definitions_default_to_automatic_permission() {
    let catalog = catalog();
    let write_name = ToolName::try_new(PLAN_WRITE_NAME.to_owned()).expect("fixture name is valid");
    let read_name = ToolName::try_new(PLAN_READ_NAME.to_owned()).expect("fixture name is valid");
    let write = catalog
        .definition(&write_name)
        .expect("write definition exists");
    let read = catalog
        .definition(&read_name)
        .expect("read definition exists");

    assert_eq!(write.permission_default(), ToolPermissionDefault::Auto);
    assert_eq!(read.permission_default(), ToolPermissionDefault::Auto);
}

#[test]
fn definitions_distinguish_read_from_write_effects() {
    let catalog = catalog();
    let write_name = ToolName::try_new(PLAN_WRITE_NAME.to_owned()).expect("fixture name is valid");
    let read_name = ToolName::try_new(PLAN_READ_NAME.to_owned()).expect("fixture name is valid");
    let write = catalog
        .definition(&write_name)
        .expect("write definition exists");
    let read = catalog
        .definition(&read_name)
        .expect("read definition exists");

    assert_eq!(write.effect_class(), ToolEffectClass::ExternalEffect);
    assert_eq!(read.effect_class(), ToolEffectClass::EffectFree);
}

#[test]
fn status_arguments_reject_values_outside_the_closed_vocabulary() {
    let catalog = catalog();
    let name = ToolName::try_new(PLAN_WRITE_NAME.to_owned()).expect("fixture name is valid");
    let unknown = arguments(json!({
        "entry_id": 1,
        "kind": "set_status",
        "status": "blocked"
    }));

    let outcome = catalog.validate_arguments(&name, &unknown);

    assert!(matches!(
        outcome,
        Err(ToolCatalogValidationFailure::InvalidArguments { detail: Some(_) })
    ));
}

#[test]
fn fold_applies_text_revision() {
    let provenance = PlanEventProvenance::from_invocation(correlation(10));
    let revised = text(REVISED_TEXT);
    let events = vec![
        event(
            1,
            provenance,
            PlanEventKind::Created {
                text: text(INITIAL_TEXT),
            },
        ),
        event(
            2,
            provenance,
            PlanEventKind::TextRevised {
                entry: entry(1),
                text: revised.clone(),
            },
        ),
    ];

    let folded = fold_plan_events(&events).expect("contiguous fixture history folds");

    assert_eq!(only_entry(&folded).text(), &revised);
}

#[test]
fn fold_applies_status_move() {
    let provenance = PlanEventProvenance::from_invocation(correlation(10));
    let events = vec![
        event(
            1,
            provenance,
            PlanEventKind::Created {
                text: text(INITIAL_TEXT),
            },
        ),
        event(
            2,
            provenance,
            PlanEventKind::StatusChanged {
                entry: entry(1),
                status: PlanStatus::InProgress,
            },
        ),
    ];

    let folded = fold_plan_events(&events).expect("contiguous fixture history folds");

    assert_eq!(only_entry(&folded).status(), PlanStatus::InProgress);
}

#[test]
fn fold_retains_an_abandoned_entry() {
    let provenance = PlanEventProvenance::from_invocation(correlation(10));
    let events = vec![
        event(
            1,
            provenance,
            PlanEventKind::Created {
                text: text(INITIAL_TEXT),
            },
        ),
        event(
            2,
            provenance,
            PlanEventKind::StatusChanged {
                entry: entry(1),
                status: PlanStatus::Abandoned,
            },
        ),
    ];

    let folded = fold_plan_events(&events).expect("contiguous fixture history folds");

    let _retained = only_entry(&folded);
}

#[test]
fn fold_preserves_creation_order_under_interleaved_entry_appends() {
    let provenance = PlanEventProvenance::from_invocation(correlation(10));
    let revised_first = text(REVISED_TEXT);
    let second = text(SECOND_TEXT);
    let events = vec![
        event(
            1,
            provenance,
            PlanEventKind::Created {
                text: text(INITIAL_TEXT),
            },
        ),
        event(
            2,
            provenance,
            PlanEventKind::Created {
                text: second.clone(),
            },
        ),
        event(
            3,
            provenance,
            PlanEventKind::StatusChanged {
                entry: entry(2),
                status: PlanStatus::InProgress,
            },
        ),
        event(
            4,
            provenance,
            PlanEventKind::TextRevised {
                entry: entry(1),
                text: revised_first.clone(),
            },
        ),
    ];

    let folded = fold_plan_events(&events).expect("interleaved fixture history folds");
    let (first, later) = ordered_pair(&folded);

    assert_eq!(first.id(), entry(1));
    assert_eq!(first.text(), &revised_first);
    assert_eq!(first.status(), PlanStatus::Pending);
    assert_eq!(later.id(), entry(2));
    assert_eq!(later.text(), &second);
    assert_eq!(later.status(), PlanStatus::InProgress);
}

#[test]
fn write_uses_trusted_provenance() {
    let dispatch = correlation(10);
    let provenance = PlanEventProvenance::from_invocation(dispatch);
    let appended = event(
        7,
        provenance,
        PlanEventKind::Created {
            text: text(INITIAL_TEXT),
        },
    );
    let port = FakePort::appending(appended);
    let (_catalog, mut executor) = PlanTools::try_new(port)
        .expect("fixture tools compile")
        .into_parts();
    let operation =
        decode_write_operation(&arguments(json!({"kind": "create", "text": INITIAL_TEXT})))
            .expect("fixture append arguments are valid");

    let _evidence =
        run_ready(executor.execute_operation(dispatch, operation)).expect("fake append succeeds");
    let port = executor.into_port();
    let observed = only_append_request(&port);

    assert_eq!(observed.session(), dispatch.session());
    assert_eq!(observed.provenance(), provenance);
}

#[test]
fn write_returns_the_assigned_entry_identity() {
    let dispatch = correlation(10);
    let provenance = PlanEventProvenance::from_invocation(dispatch);
    let appended = event(
        7,
        provenance,
        PlanEventKind::Created {
            text: text(INITIAL_TEXT),
        },
    );
    let expected_ordinal = appended.ordinal();
    let port = FakePort::appending(appended);
    let (_catalog, mut executor) = PlanTools::try_new(port)
        .expect("fixture tools compile")
        .into_parts();
    let operation =
        decode_write_operation(&arguments(json!({"kind": "create", "text": INITIAL_TEXT})))
            .expect("fixture append arguments are valid");

    let evidence =
        run_ready(executor.execute_operation(dispatch, operation)).expect("fake append succeeds");
    let output: Value =
        serde_json::from_str(&completed_text(evidence)).expect("tool result is compact JSON");

    assert_eq!(output["event"]["ordinal"], json!(expected_ordinal.as_u64()));
    assert_eq!(
        output["event"]["entry_id"],
        json!(PlanEntryId::from_creation_ordinal(expected_ordinal).as_u64())
    );
}

#[test]
fn read_reports_long_history_truncation_without_implying_completeness() {
    let dispatch = correlation(10);
    let provenance = PlanEventProvenance::from_invocation(dispatch);
    let current = PlanEntry::new(entry(1), text(REVISED_TEXT), PlanStatus::Completed);
    let history = PlanHistoryPage::new(
        vec![event(
            1,
            provenance,
            PlanEventKind::Created {
                text: text(INITIAL_TEXT),
            },
        )],
        PlanPageCompleteness::Truncated,
    );
    let port = FakePort::reading(PlanReadPage::new(
        dispatch.session(),
        vec![current],
        PlanPageCompleteness::Complete,
        Some(history),
    ));
    let (_catalog, mut executor) = PlanTools::try_new(port)
        .expect("fixture tools compile")
        .into_parts();
    let operation = decode_read_operation(&arguments(json!({"include_history": true})))
        .expect("fixture history arguments are valid");

    let evidence = run_ready(executor.execute_operation(dispatch, operation))
        .expect("fake history read succeeds");
    let output: Value =
        serde_json::from_str(&completed_text(evidence)).expect("tool result is compact JSON");
    let port = executor.into_port();
    let observed = only_read_request(&port);

    assert_eq!(observed.session(), dispatch.session());
    assert_eq!(observed.history_limit(), Some(MAX_PLAN_HISTORY_EVENTS));
    assert_eq!(output["history_truncated"], json!(true));
    assert_eq!(output["history"].as_array().map(Vec::len), Some(1));
}

#[test]
fn direct_read_request_bounds_history_to_the_declared_page_limit() {
    let dispatch = correlation(10);
    let request = PlanReadRequest::new(dispatch.session(), None, Some(usize::MAX));

    assert_eq!(request.history_limit(), Some(MAX_PLAN_HISTORY_EVENTS));
}

#[test]
fn direct_read_request_lifts_history_to_the_declared_page_minimum() {
    let dispatch = correlation(10);
    let request = PlanReadRequest::new(dispatch.session(), None, Some(0));

    assert_eq!(request.history_limit(), Some(MIN_PLAN_HISTORY_EVENTS));
}

#[test]
fn text_rejects_postgres_null_at_the_checked_boundary() {
    let rejected = PlanText::try_new("cannot\0persist".to_owned());

    assert_eq!(rejected, Err(PlanTextError::ContainsNull));
}

#[test]
fn write_reports_unknown_entry_as_an_authenticated_failure() {
    let dispatch = correlation(10);
    let missing = entry(8);
    let port = FakePort::rejecting(PlanAppendRejection::UnknownEntry { entry: missing });
    let (_catalog, mut executor) = PlanTools::try_new(port)
        .expect("fixture tools compile")
        .into_parts();
    let operation = decode_write_operation(&arguments(json!({
        "entry_id": missing.as_u64(),
        "kind": "set_status",
        "status": "completed"
    })))
    .expect("fixture mutation arguments are valid");

    let evidence = run_ready(executor.execute_operation(dispatch, operation))
        .expect("unknown entry is a known failure");
    let port = executor.into_port();
    let observed = only_append_request(&port);

    assert!(known_failure_has_detail(&evidence));
    assert_eq!(observed.session(), dispatch.session());
}

#[test]
fn write_rejects_a_port_event_that_precedes_its_mutation_target() {
    let dispatch = correlation(10);
    let provenance = PlanEventProvenance::from_invocation(dispatch);
    let target = entry(2);
    let appended = event(
        1,
        provenance,
        PlanEventKind::TextRevised {
            entry: target,
            text: text(REVISED_TEXT),
        },
    );
    let port = FakePort::appending(appended);
    let (_catalog, mut executor) = PlanTools::try_new(port)
        .expect("fixture tools compile")
        .into_parts();
    let operation = decode_write_operation(&arguments(json!({
        "entry_id": target.as_u64(),
        "kind": "revise",
        "text": REVISED_TEXT
    })))
    .expect("fixture revision arguments are valid");

    let error = run_ready(executor.execute_operation(dispatch, operation))
        .expect_err("self-targeting durable mutation violates the port contract");

    assert!(is_port_contract(&error));
}

#[test]
fn read_truncates_oversized_encoded_evidence_and_reports_every_omission() {
    let dispatch = correlation(10);
    let provenance = PlanEventProvenance::from_invocation(dispatch);
    let port = FakePort::reading(oversized_read_page(provenance));
    let (_catalog, mut executor) = PlanTools::try_new(port)
        .expect("fixture tools compile")
        .into_parts();
    let operation = decode_read_operation(&arguments(json!({"include_history": true})))
        .expect("fixture history arguments are valid");

    let evidence = run_ready(executor.execute_operation(dispatch, operation))
        .expect("oversized evidence is honestly truncated");
    let encoded = completed_text(evidence);
    let output: Value = serde_json::from_str(&encoded).expect("tool result is compact JSON");

    assert!(ToolResultText::try_new(encoded).is_ok());
    assert_eq!(output["plan_truncated"], json!(true));
    assert_eq!(output["history_truncated"], json!(true));
}

#[test]
fn read_rejects_repeated_tool_attempt_provenance() {
    let dispatch = correlation(10);
    let provenance = PlanEventProvenance::from_invocation(dispatch);
    let initial = text(INITIAL_TEXT);
    let second = text(SECOND_TEXT);
    let history = PlanHistoryPage::new(
        vec![
            event(
                1,
                provenance,
                PlanEventKind::Created {
                    text: initial.clone(),
                },
            ),
            event(
                2,
                provenance,
                PlanEventKind::Created {
                    text: second.clone(),
                },
            ),
        ],
        PlanPageCompleteness::Complete,
    );
    let current = vec![
        PlanEntry::new(entry(1), initial, PlanStatus::Pending),
        PlanEntry::new(entry(2), second, PlanStatus::Pending),
    ];
    let port = FakePort::reading(PlanReadPage::new(
        dispatch.session(),
        current,
        PlanPageCompleteness::Complete,
        Some(history),
    ));
    let (_catalog, mut executor) = PlanTools::try_new(port)
        .expect("fixture tools compile")
        .into_parts();
    let operation = decode_read_operation(&arguments(json!({"include_history": true})))
        .expect("fixture history arguments are valid");

    let error = run_ready(executor.execute_operation(dispatch, operation))
        .expect_err("repeated physical attempt violates the port contract");

    assert!(is_port_contract(&error));
}

#[test]
fn read_rejects_current_page_from_another_session() {
    let dispatch = correlation(10);
    let foreign_session = correlation(20).session();
    let current = PlanEntry::new(entry(1), text(INITIAL_TEXT), PlanStatus::Pending);
    let port = FakePort::reading(PlanReadPage::new(
        foreign_session,
        vec![current],
        PlanPageCompleteness::Complete,
        None,
    ));
    let (_catalog, mut executor) = PlanTools::try_new(port)
        .expect("fixture tools compile")
        .into_parts();
    let operation =
        decode_read_operation(&arguments(json!({}))).expect("fixture read arguments are valid");

    let error = run_ready(executor.execute_operation(dispatch, operation))
        .expect_err("foreign current page violates the port contract");

    assert!(is_port_contract(&error));
}

#[test]
fn read_rejects_current_entries_that_contradict_complete_history() {
    let dispatch = correlation(10);
    let provenance = PlanEventProvenance::from_invocation(dispatch);
    let history = PlanHistoryPage::new(
        vec![event(
            1,
            provenance,
            PlanEventKind::Created {
                text: text(INITIAL_TEXT),
            },
        )],
        PlanPageCompleteness::Complete,
    );
    let contradictory = PlanEntry::new(entry(1), text(REVISED_TEXT), PlanStatus::Pending);
    let port = FakePort::reading(PlanReadPage::new(
        dispatch.session(),
        vec![contradictory],
        PlanPageCompleteness::Complete,
        Some(history),
    ));
    let (_catalog, mut executor) = PlanTools::try_new(port)
        .expect("fixture tools compile")
        .into_parts();
    let operation = decode_read_operation(&arguments(json!({"include_history": true})))
        .expect("fixture history arguments are valid");

    let error = run_ready(executor.execute_operation(dispatch, operation))
        .expect_err("contradictory current state violates the port contract");

    assert!(is_port_contract(&error));
}

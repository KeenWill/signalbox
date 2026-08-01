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
    append_result: Option<PlanEvent>,
    read_result: Option<PlanReadPage>,
    append_requests: Vec<PlanAppendRequest>,
    read_requests: Vec<PlanReadRequest>,
}

impl FakePort {
    fn appending(result: PlanEvent) -> Self {
        Self {
            append_result: Some(result),
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
    ) -> impl Future<Output = Result<PlanEvent, Self::Error>> + Send {
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
    let ToolExecutorEvidence::CompletedText(result) = evidence else {
        panic!("fixture execution completes with text")
    };
    result
}

#[test]
fn definitions_are_automatic_and_distinguish_read_from_write_effects() {
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
fn fold_applies_revisions_and_status_moves_without_removing_abandoned_entries() {
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
        event(
            3,
            provenance,
            PlanEventKind::StatusChanged {
                entry: entry(1),
                status: PlanStatus::InProgress,
            },
        ),
        event(
            4,
            provenance,
            PlanEventKind::StatusChanged {
                entry: entry(1),
                status: PlanStatus::Abandoned,
            },
        ),
    ];

    let folded = fold_plan_events(&events).expect("contiguous fixture history folds");
    let [current] = folded.entries() else {
        panic!("one created entry remains visible")
    };

    assert_eq!(current.id(), entry(1));
    assert_eq!(current.text(), &revised);
    assert_eq!(current.status(), PlanStatus::Abandoned);
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
    let [first, later] = folded.entries() else {
        panic!("two entries remain in creation order")
    };

    assert_eq!(first.id(), entry(1));
    assert_eq!(first.text(), &revised_first);
    assert_eq!(first.status(), PlanStatus::Pending);
    assert_eq!(later.id(), entry(2));
    assert_eq!(later.text(), &second);
    assert_eq!(later.status(), PlanStatus::InProgress);
}

#[test]
fn write_uses_trusted_provenance_and_returns_the_assigned_entry_identity() {
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
    let operation = decode_operation(
        PlanToolKind::Write,
        &arguments(json!({"kind": "create", "text": INITIAL_TEXT})),
    )
    .expect("fixture append arguments are valid");

    let evidence =
        run_ready(executor.execute_operation(dispatch, operation)).expect("fake append succeeds");
    let output: Value =
        serde_json::from_str(&completed_text(evidence)).expect("tool result is compact JSON");
    let port = executor.into_port();
    let [observed] = port.append_requests.as_slice() else {
        panic!("one append request is observed")
    };

    assert_eq!(observed.session(), dispatch.session());
    assert_eq!(observed.provenance(), provenance);
    assert_eq!(output["event"]["ordinal"], json!(7));
    assert_eq!(output["event"]["entry_id"], json!(7));
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
        true,
    );
    let port = FakePort::reading(PlanReadPage::new(vec![current], false, Some(history)));
    let (_catalog, mut executor) = PlanTools::try_new(port)
        .expect("fixture tools compile")
        .into_parts();
    let operation = decode_operation(
        PlanToolKind::Read,
        &arguments(json!({"include_history": true})),
    )
    .expect("fixture history arguments are valid");

    let evidence = run_ready(executor.execute_operation(dispatch, operation))
        .expect("fake history read succeeds");
    let output: Value =
        serde_json::from_str(&completed_text(evidence)).expect("tool result is compact JSON");
    let port = executor.into_port();
    let [observed] = port.read_requests.as_slice() else {
        panic!("one read request is observed")
    };

    assert_eq!(observed.session(), dispatch.session());
    assert_eq!(observed.history_limit(), Some(MAX_PLAN_HISTORY_EVENTS));
    assert_eq!(output["history_truncated"], json!(true));
    assert_eq!(output["history"].as_array().map(Vec::len), Some(1));
}

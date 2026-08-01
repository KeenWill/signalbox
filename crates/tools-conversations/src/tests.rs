use std::{
    error::Error,
    fmt,
    future::{Future, ready},
    pin::pin,
    task::{Context, Poll, Waker},
};

use serde_json::{Value, json};
use signalbox_application::{ToolCatalog, ToolCatalogValidationFailure};
use signalbox_domain::ToolName;
use signalbox_tool_contract::rendered_contract_schema;

use super::*;

const VISIBLE_CONTENT: &str = "visible transcript content";
const REDACTED_CONTENT: &str = "[REDACTED]";
const HIDDEN_SECRET: &str = "credential-that-must-stay-hidden";

fn session(value: u128) -> SessionId {
    SessionId::from_uuid(uuid::Uuid::from_u128(value))
}

fn imported(value: u128) -> ImportedConversationId {
    ImportedConversationId::from_uuid(uuid::Uuid::from_u128(value))
}

fn position(value: u64) -> NonZeroU64 {
    NonZeroU64::new(value).expect("fixture transcript position is positive")
}

fn arguments(value: String) -> NormalizedToolArguments {
    NormalizedToolArguments::try_from_provider_text(value).expect("fixture arguments are admitted")
}

fn own_transcript_arguments() -> NormalizedToolArguments {
    arguments(
        json!({
            "after_position": 3,
            "max_bytes": VISIBLE_CONTENT.len(),
            "max_entries": 1,
        })
        .to_string(),
    )
}

fn selected_transcript_arguments(target: SessionId) -> NormalizedToolArguments {
    arguments(
        json!({
            "after_position": 3,
            "max_bytes": VISIBLE_CONTENT.len(),
            "max_entries": 1,
            "session_id": target.into_uuid().to_string(),
        })
        .to_string(),
    )
}

fn one_entry_page(content: &str, content_truncated: bool, has_more: bool) -> TranscriptPage {
    TranscriptPage::new(
        vec![TranscriptEntry::new(
            position(4),
            TranscriptEntryKind::Assistant,
            content.to_owned(),
            content_truncated,
        )],
        has_more,
    )
}

fn run_ready<Output>(future: impl Future<Output = Output>) -> Output {
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);
    let mut future = pin!(future);
    match future.as_mut().poll(&mut context) {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("fake-backed tool execution must be immediately ready"),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FakeError;

impl fmt::Display for FakeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("fake conversation port failed")
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
    list_result: Option<ConversationListPage>,
    native_result: Option<ConversationTranscriptRead>,
    imported_result: Option<Option<TranscriptPage>>,
    list_requests: Vec<ConversationListRequest>,
    native_requests: Vec<ConversationTranscriptRequest>,
    imported_requests: Vec<ImportedTranscriptRequest>,
}

impl FakePort {
    fn listing(page: ConversationListPage) -> Self {
        Self {
            list_result: Some(page),
            native_result: None,
            imported_result: None,
            list_requests: Vec::new(),
            native_requests: Vec::new(),
            imported_requests: Vec::new(),
        }
    }

    fn reading_native(page: Option<TranscriptPage>) -> Self {
        let read = match page {
            Some(page) => ConversationTranscriptRead::Read(page),
            None => ConversationTranscriptRead::NotFound,
        };
        Self {
            list_result: None,
            native_result: Some(read),
            imported_result: None,
            list_requests: Vec::new(),
            native_requests: Vec::new(),
            imported_requests: Vec::new(),
        }
    }

    fn refusing_native(refusal: SessionReadScopeRefusal) -> Self {
        Self {
            list_result: None,
            native_result: Some(ConversationTranscriptRead::Refused(refusal)),
            imported_result: None,
            list_requests: Vec::new(),
            native_requests: Vec::new(),
            imported_requests: Vec::new(),
        }
    }

    fn reading_imported(page: Option<TranscriptPage>) -> Self {
        Self {
            list_result: None,
            native_result: None,
            imported_result: Some(page),
            list_requests: Vec::new(),
            native_requests: Vec::new(),
            imported_requests: Vec::new(),
        }
    }
}

impl ConversationIntrospectionPort for FakePort {
    type Error = FakeError;

    fn list_conversations(
        &mut self,
        request: ConversationListRequest,
    ) -> impl Future<Output = Result<ConversationListPage, Self::Error>> + Send {
        self.list_requests.push(request);
        ready(Ok(self
            .list_result
            .take()
            .expect("fixture list operation was configured")))
    }

    fn read_conversation(
        &mut self,
        request: ConversationTranscriptRequest,
    ) -> impl Future<Output = Result<ConversationTranscriptRead, Self::Error>> + Send {
        self.native_requests.push(request);
        ready(Ok(self
            .native_result
            .take()
            .expect("fixture native read was configured")))
    }

    fn read_imported_conversation(
        &mut self,
        request: ImportedTranscriptRequest,
    ) -> impl Future<Output = Result<Option<TranscriptPage>, Self::Error>> + Send {
        self.imported_requests.push(request);
        ready(Ok(self
            .imported_result
            .take()
            .expect("fixture imported read was configured")))
    }
}

#[track_caller]
fn assert_definition(catalog: &CompiledToolCatalog, name: &str, permission: ToolPermissionDefault) {
    let definition = catalog
        .definition(&ToolName::try_new(name.to_owned()).expect("fixture name is admitted"))
        .expect("fixture definition exists");
    assert_eq!(definition.permission_default(), permission);
    assert_eq!(definition.effect_class(), ToolEffectClass::EffectFree);
}

fn catalog() -> CompiledToolCatalog {
    ConversationTools::try_new(FakePort::reading_native(None))
        .expect("static conversation tools compile")
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
fn definitions_encode_own_auto_and_cross_conversation_confirmation() {
    let catalog = catalog();

    assert_definition(
        &catalog,
        LIST_CONVERSATIONS_NAME,
        ToolPermissionDefault::Confirm,
    );
    assert_definition(
        &catalog,
        READ_OWN_CONVERSATION_NAME,
        ToolPermissionDefault::Auto,
    );
    assert_definition(
        &catalog,
        READ_CONVERSATION_NAME,
        ToolPermissionDefault::Confirm,
    );
    assert_definition(
        &catalog,
        READ_IMPORTED_CONVERSATION_NAME,
        ToolPermissionDefault::Confirm,
    );
}

#[test]
fn transcript_schemas_carry_required_entry_and_byte_bounds() {
    let own = rendered_contract_schema::<ReadOwnConversationContract>();
    let other = rendered_contract_schema::<ReadConversationContract>();
    let imported = rendered_contract_schema::<ReadImportedConversationContract>();

    assert_eq!(own["properties"]["max_entries"]["minimum"], json!(1));
    assert_eq!(
        own["properties"]["max_entries"]["maximum"],
        json!(MAX_TRANSCRIPT_ENTRIES)
    );
    assert_eq!(own["properties"]["max_bytes"]["minimum"], json!(1));
    assert_eq!(
        own["properties"]["max_bytes"]["maximum"],
        json!(MAX_TRANSCRIPT_CONTENT_BYTES)
    );
    assert_eq!(own["required"], json!(["max_entries", "max_bytes"]));
    assert_eq!(
        other["required"],
        json!(["session_id", "max_entries", "max_bytes"])
    );
    assert_eq!(
        imported["required"],
        json!(["imported_conversation_id", "max_entries", "max_bytes"])
    );
}

#[test]
fn own_read_uses_only_the_trusted_invoking_session() {
    let invoking = session(11);
    let port = FakePort::reading_native(Some(one_entry_page(VISIBLE_CONTENT, false, false)));
    let (_catalog, mut executor) = ConversationTools::try_new(port)
        .expect("fixture tools compile")
        .into_parts();
    let operation = decode_operation(ConversationToolKind::ReadOwn, &own_transcript_arguments())
        .expect("bounded own-read arguments are valid");

    let evidence =
        run_ready(executor.execute_operation(invoking, operation)).expect("fake own read succeeds");
    let output: Value =
        serde_json::from_str(&completed_text(evidence)).expect("tool result is compact JSON");
    let port = executor.into_port();
    let [observed] = port.native_requests.as_slice() else {
        panic!("one native request is observed")
    };

    assert_eq!(observed.requesting_session(), invoking);
    assert_eq!(observed.target_session(), invoking);
    assert_eq!(observed.after_position(), Some(position(3)));
    assert_eq!(observed.max_entries(), 1);
    assert_eq!(observed.max_bytes(), VISIBLE_CONTENT.len());
    assert_eq!(output["session_id"], invoking.into_uuid().to_string());
}

#[test]
fn selected_native_read_forwards_the_model_named_target() {
    let invoking = session(11);
    let selected = session(22);
    let port = FakePort::reading_native(Some(one_entry_page(VISIBLE_CONTENT, false, false)));
    let (_catalog, mut executor) = ConversationTools::try_new(port)
        .expect("fixture tools compile")
        .into_parts();
    let operation = decode_operation(
        ConversationToolKind::ReadOther,
        &selected_transcript_arguments(selected),
    )
    .expect("bounded selected-read arguments are valid");

    let _evidence = run_ready(executor.execute_operation(invoking, operation))
        .expect("fake selected read succeeds");
    let port = executor.into_port();
    let [observed] = port.native_requests.as_slice() else {
        panic!("one native request is observed")
    };

    assert_eq!(observed.requesting_session(), invoking);
    assert_eq!(observed.target_session(), selected);
}

#[test]
fn selected_native_read_returns_typed_out_of_scope_evidence() {
    let invoking = session(11);
    let selected = session(22);
    let requester = signalbox_domain::SessionPlacement::scoped(
        signalbox_domain::SessionPlacementPath::try_new(String::from("projects.foo.reviews.pr123"))
            .expect("fixture path is admitted"),
    )
    .expect("fixture path is non-root");
    let target = signalbox_domain::SessionPlacement::scoped(
        signalbox_domain::SessionPlacementPath::try_new(String::from("projects.bar.session"))
            .expect("fixture path is admitted"),
    )
    .expect("fixture path is non-root");
    let signalbox_domain::SessionReadScopeDecision::Refused(refusal) =
        requester.decide_cross_session_read(&target)
    else {
        panic!("fixture placements are disjoint")
    };
    let expected_directory = refusal.requesting_directory().as_str().to_owned();
    let port = FakePort::refusing_native(refusal);
    let (_catalog, mut executor) = ConversationTools::try_new(port)
        .expect("fixture tools compile")
        .into_parts();
    let operation = decode_operation(
        ConversationToolKind::ReadOther,
        &selected_transcript_arguments(selected),
    )
    .expect("bounded selected-read arguments are valid");

    let evidence = run_ready(executor.execute_operation(invoking, operation))
        .expect("typed refusal is a trusted tool outcome");
    let ToolExecutorEvidence::KnownFailed {
        detail: Some(detail),
    } = evidence
    else {
        panic!("scoped refusal is a known failure")
    };

    assert!(detail.as_str().contains(&expected_directory));
    assert!(
        detail
            .as_str()
            .contains("outside_requesting_directory_subtree")
    );
}

#[test]
fn list_forwards_the_explicit_bound_and_cursor() {
    let invoking = session(11);
    let after_identity = imported(20);
    let after = ConversationCursor::Imported(after_identity);
    let listed = ConversationListItem::Native {
        session: session(21),
        title: Some(String::from("visible title")),
        archived: false,
    };
    let page = ConversationListPage::new(vec![listed.clone()], true);
    let port = FakePort::listing(page);
    let (_catalog, mut executor) = ConversationTools::try_new(port)
        .expect("fixture tools compile")
        .into_parts();
    let operation = decode_operation(
        ConversationToolKind::List,
        &arguments(
            json!({
                "after": {
                    "id": after_identity.into_uuid().to_string(),
                    "kind": "imported",
                },
                "max_results": 1,
            })
            .to_string(),
        ),
    )
    .expect("bounded list arguments are valid");

    let evidence =
        run_ready(executor.execute_operation(invoking, operation)).expect("fake list succeeds");
    let output: Value =
        serde_json::from_str(&completed_text(evidence)).expect("tool result is compact JSON");
    let port = executor.into_port();
    let [observed] = port.list_requests.as_slice() else {
        panic!("one list request is observed")
    };

    assert_eq!(observed.after(), Some(after));
    assert_eq!(observed.max_results(), 1);
    assert_eq!(
        output["next_after"]["id"],
        listed.cursor().identity_uuid().to_string()
    );
    assert_eq!(output["truncated"], json!(true));
}

#[test]
fn imported_read_forwards_the_selected_target_and_bounds() {
    let invoking = session(11);
    let selected = imported(22);
    let port = FakePort::reading_imported(Some(one_entry_page(VISIBLE_CONTENT, false, false)));
    let (_catalog, mut executor) = ConversationTools::try_new(port)
        .expect("fixture tools compile")
        .into_parts();
    let operation = decode_operation(
        ConversationToolKind::ReadImported,
        &arguments(
            json!({
                "after_position": 3,
                "imported_conversation_id": selected.into_uuid().to_string(),
                "max_bytes": VISIBLE_CONTENT.len(),
                "max_entries": 1,
            })
            .to_string(),
        ),
    )
    .expect("bounded imported-read arguments are valid");

    let evidence = run_ready(executor.execute_operation(invoking, operation))
        .expect("fake imported read succeeds");
    let output: Value =
        serde_json::from_str(&completed_text(evidence)).expect("tool result is compact JSON");
    let port = executor.into_port();
    let [observed] = port.imported_requests.as_slice() else {
        panic!("one imported request is observed")
    };

    assert_eq!(observed.conversation(), selected);
    assert_eq!(observed.after_position(), Some(position(3)));
    assert_eq!(observed.max_entries(), 1);
    assert_eq!(observed.max_bytes(), VISIBLE_CONTENT.len());
    assert_eq!(
        output["imported_conversation_id"],
        selected.into_uuid().to_string()
    );
}

#[test]
fn later_entries_produce_an_honest_continuation_and_truncation_signal() {
    let invoking = session(11);
    let returned_position = position(4);
    let port = FakePort::reading_native(Some(one_entry_page(VISIBLE_CONTENT, false, true)));
    let (_catalog, mut executor) = ConversationTools::try_new(port)
        .expect("fixture tools compile")
        .into_parts();
    let operation = decode_operation(ConversationToolKind::ReadOwn, &own_transcript_arguments())
        .expect("bounded own-read arguments are valid");

    let evidence =
        run_ready(executor.execute_operation(invoking, operation)).expect("fake own read succeeds");
    let output: Value =
        serde_json::from_str(&completed_text(evidence)).expect("tool result is compact JSON");

    assert_eq!(output["next_after"], json!(returned_position.get()));
    assert_eq!(output["truncated"], json!(true));
    assert_eq!(output["entries"][0]["content_truncated"], json!(false));
}

#[test]
fn partial_entry_content_produces_an_honest_truncation_signal() {
    let invoking = session(11);
    let port = FakePort::reading_native(Some(one_entry_page(VISIBLE_CONTENT, true, false)));
    let (_catalog, mut executor) = ConversationTools::try_new(port)
        .expect("fixture tools compile")
        .into_parts();
    let operation = decode_operation(ConversationToolKind::ReadOwn, &own_transcript_arguments())
        .expect("bounded own-read arguments are valid");

    let evidence =
        run_ready(executor.execute_operation(invoking, operation)).expect("fake own read succeeds");
    let output: Value =
        serde_json::from_str(&completed_text(evidence)).expect("tool result is compact JSON");

    assert_eq!(output["next_after"], Value::Null);
    assert_eq!(output["truncated"], json!(true));
    assert_eq!(output["entries"][0]["content_truncated"], json!(true));
}

#[test]
fn redaction_projection_has_no_bypass_and_hidden_material_stays_hidden() {
    let invoking = session(11);
    let selected = session(22);
    let catalog = catalog();
    let name = ToolName::try_new(String::from(READ_CONVERSATION_NAME))
        .expect("fixture tool name is admitted");
    let bypass = arguments(
        json!({
            "after_position": null,
            "include_redacted": true,
            "max_bytes": MAX_TRANSCRIPT_CONTENT_BYTES,
            "max_entries": MAX_TRANSCRIPT_ENTRIES,
            "session_id": selected.into_uuid().to_string(),
        })
        .to_string(),
    );
    let port = RedactingPort {
        hidden_source: String::from(HIDDEN_SECRET),
    };
    let (_catalog, mut executor) = ConversationTools::try_new(port)
        .expect("fixture tools compile")
        .into_parts();
    let operation = decode_operation(
        ConversationToolKind::ReadOther,
        &selected_transcript_arguments(selected),
    )
    .expect("bounded selected-read arguments are valid");

    let evidence = run_ready(executor.execute_operation(invoking, operation))
        .expect("redacted projection read succeeds");
    let output = completed_text(evidence);
    let port = executor.into_port();

    assert!(matches!(
        catalog.validate_arguments(&name, &bypass),
        Err(ToolCatalogValidationFailure::InvalidArguments { detail: Some(_) })
    ));
    assert!(output.contains(REDACTED_CONTENT));
    assert!(!output.contains(port.hidden_source.as_str()));
}

#[derive(Debug)]
struct RedactingPort {
    hidden_source: String,
}

impl ConversationIntrospectionPort for RedactingPort {
    type Error = FakeError;

    async fn list_conversations(
        &mut self,
        _request: ConversationListRequest,
    ) -> Result<ConversationListPage, Self::Error> {
        panic!("redaction fixture does not list conversations")
    }

    fn read_conversation(
        &mut self,
        _request: ConversationTranscriptRequest,
    ) -> impl Future<Output = Result<ConversationTranscriptRead, Self::Error>> + Send {
        ready(Ok(ConversationTranscriptRead::Read(one_entry_page(
            REDACTED_CONTENT,
            false,
            false,
        ))))
    }

    async fn read_imported_conversation(
        &mut self,
        _request: ImportedTranscriptRequest,
    ) -> Result<Option<TranscriptPage>, Self::Error> {
        panic!("redaction fixture does not read imported conversations")
    }
}

#[test]
fn port_cannot_exceed_the_requested_content_byte_bound() {
    let invoking = session(11);
    let port = FakePort::reading_native(Some(one_entry_page(
        "visible transcript content beyond bound",
        false,
        false,
    )));
    let (_catalog, mut executor) = ConversationTools::try_new(port)
        .expect("fixture tools compile")
        .into_parts();
    let operation = decode_operation(ConversationToolKind::ReadOwn, &own_transcript_arguments())
        .expect("bounded own-read arguments are valid");

    let error = run_ready(executor.execute_operation(invoking, operation))
        .expect_err("over-bound port page is rejected");

    assert!(matches!(error, ConversationExecutorError::PortContract));
}

#[test]
fn catalog_rejects_zero_and_over_maximum_transcript_bounds() {
    let catalog = catalog();
    let name = ToolName::try_new(String::from(READ_OWN_CONVERSATION_NAME))
        .expect("fixture tool name is admitted");
    let zero =
        arguments(json!({"after_position": null, "max_bytes": 0, "max_entries": 1}).to_string());
    let over_maximum = arguments(
        json!({
            "after_position": null,
            "max_bytes": MAX_TRANSCRIPT_CONTENT_BYTES,
            "max_entries": MAX_TRANSCRIPT_ENTRIES + 1,
        })
        .to_string(),
    );

    assert!(matches!(
        catalog.validate_arguments(&name, &zero),
        Err(ToolCatalogValidationFailure::InvalidArguments { detail: Some(_) })
    ));
    assert!(matches!(
        catalog.validate_arguments(&name, &over_maximum),
        Err(ToolCatalogValidationFailure::InvalidArguments { detail: Some(_) })
    ));
}

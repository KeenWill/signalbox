use super::{
    Arc, ContextFrontierId, DangerousToolAutoApproval, FakePrepare, FixedIds,
    InProcessAttemptDispatchGate, InitialToolApproval, ModelCallExecutionService,
    ModelCallTerminalIdentityCandidates, ModelCallTerminalObservation, NoToolCatalog,
    SemanticTranscriptEntryId, StoppedToolResponsePartIdentity,
    StoppedToolRoundModelCallIdentities, ToolRequestId, ToolResponsePartIdentity,
    UnusedAuthorization, UnusedFailure, UnusedObservation, UnusedProvider, VecDeque,
    guarded_proposal, guarded_tool_approvals, identity, recorded_guarded_override, tool_response,
};

/// a recorded override substitutes for the judge only on the exact denied command — a proposal
/// with other arguments still parks for the judge — and the selected approval carries the
/// override command and the overridden denial.
#[test]
fn recorded_override_substitutes_for_the_judge_on_the_exact_command() {
    let recorded = recorded_guarded_override();
    let approvals = guarded_tool_approvals(
        signalbox_domain::ToolApprovalPosture::Delegated,
        vec![
            guarded_proposal("{}"),
            guarded_proposal(r#"{"timezone":"UTC"}"#),
        ],
        std::slice::from_ref(&recorded),
    );

    assert_eq!(
        approvals.as_ref(),
        [
            InitialToolApproval::UserOverride {
                command: recorded.command(),
                denied_request: recorded.denied_request(),
            },
            InitialToolApproval::Delegated,
        ]
    );
}

/// one recorded override pre-approves at most one proposal per response; a second identical
/// proposal parks for the judge again.
#[test]
fn recorded_override_is_consumed_at_most_once_per_response() {
    let recorded = recorded_guarded_override();
    let approvals = guarded_tool_approvals(
        signalbox_domain::ToolApprovalPosture::Delegated,
        vec![guarded_proposal("{}"), guarded_proposal("{}")],
        std::slice::from_ref(&recorded),
    );

    assert_eq!(
        approvals.as_ref(),
        [
            InitialToolApproval::UserOverride {
                command: recorded.command(),
                denied_request: recorded.denied_request(),
            },
            InitialToolApproval::Delegated,
        ]
    );
}

/// a recorded override substitutes only where the judge would decide; a human-frozen selection
/// is never overridden.
#[test]
fn recorded_override_never_bypasses_a_human_selection() {
    let approvals = guarded_tool_approvals(
        signalbox_domain::ToolApprovalPosture::Human,
        vec![guarded_proposal("{}")],
        &[recorded_guarded_override()],
    );

    assert_eq!(approvals.as_ref(), [InitialToolApproval::Human]);
}

/// one identity is minted per ordered response part/request, approval stays pinned to the
/// advertised catalog snapshot, mixed auto/confirm policy parks without a continuation attempt,
/// and the adapter still receives a stopped race closure.
#[test]
fn tool_response_candidates_preserve_order_and_policy() {
    let schema =
        crate::ToolInputSchema::try_new(String::from(r#"{"properties":{},"type":"object"}"#))
            .expect("fixture schema is valid");
    let definition = crate::ToolDefinition::new(
        signalbox_domain::ToolName::try_new(String::from("automatic"))
            .expect("fixture name is valid"),
        String::from("Runs automatically."),
        schema,
        signalbox_domain::ToolPermissionDefault::Auto,
        signalbox_domain::ToolEffectClass::EffectFree,
    );
    let catalog = crate::CompiledToolCatalog::try_new([crate::CompiledTool::new(
        definition,
        |_: &signalbox_domain::NormalizedToolArguments| Ok(()),
    )])
    .expect("one tool is unambiguous");
    let mut service = ModelCallExecutionService::new(
        FixedIds::baseline(),
        FakePrepare {
            outcomes: VecDeque::new(),
            calls: 0,
        },
        UnusedFailure,
        UnusedAuthorization,
        UnusedObservation,
        UnusedProvider,
        InProcessAttemptDispatchGate::default(),
        None,
    )
    .with_tool_catalog(catalog);
    let observation = tool_response();
    let advertised_tools = service.catalog.definitions();
    service.catalog = Arc::new(NoToolCatalog);
    let approvals = service.tool_approvals(
        &observation,
        DangerousToolAutoApproval::Disabled,
        &advertised_tools,
        &[],
    );
    assert_eq!(
        approvals.as_ref(),
        [
            InitialToolApproval::PolicyAuto,
            InitialToolApproval::Confirm
        ]
    );

    let ModelCallTerminalIdentityCandidates::ToolRound {
        continuing,
        stopped,
    } = service.next_terminal_identities(&observation, &approvals)
    else {
        panic!("tool response requires both race-safe closures");
    };
    assert_eq!(continuing.response_parts().len(), 3);
    assert_eq!(continuing.continuation_attempt(), None);
    let [
        ToolResponsePartIdentity::Text { .. },
        ToolResponsePartIdentity::ToolCall {
            approval: first_approval,
            ..
        },
        ToolResponsePartIdentity::ToolCall {
            approval: second_approval,
            ..
        },
    ] = continuing.response_parts()
    else {
        panic!("fixture response preserves one text part then two tool calls");
    };
    assert_eq!(*first_approval, InitialToolApproval::PolicyAuto);
    assert_eq!(*second_approval, InitialToolApproval::Confirm);

    let non_overridable_approvals = [
        InitialToolApproval::PolicyAuto,
        InitialToolApproval::AlwaysConfirm,
    ];
    service.ids = FixedIds::baseline();
    let ModelCallTerminalIdentityCandidates::ToolRound { continuing, .. } =
        service.next_terminal_identities(&observation, &non_overridable_approvals)
    else {
        panic!("tool response requires both race-safe closures");
    };
    assert_eq!(continuing.continuation_attempt(), None);
    assert_eq!(
        stopped,
        StoppedToolRoundModelCallIdentities::new(
            vec![
                StoppedToolResponsePartIdentity::text(identity(
                    33,
                    SemanticTranscriptEntryId::from_uuid,
                )),
                StoppedToolResponsePartIdentity::tool_call(
                    identity(34, SemanticTranscriptEntryId::from_uuid),
                    identity(62, ToolRequestId::from_uuid),
                    identity(35, SemanticTranscriptEntryId::from_uuid),
                    InitialToolApproval::PolicyAuto,
                ),
                StoppedToolResponsePartIdentity::tool_call(
                    identity(36, SemanticTranscriptEntryId::from_uuid),
                    identity(63, ToolRequestId::from_uuid),
                    identity(37, SemanticTranscriptEntryId::from_uuid),
                    InitialToolApproval::Confirm,
                ),
            ],
            identity(38, SemanticTranscriptEntryId::from_uuid),
            identity(41, ContextFrontierId::from_uuid),
        ),
        "lifecycle-dependent candidates receive a disjoint identity inventory"
    );
}

/// a credential-suppressed proposal bypasses the advertised execution policy and receives an
/// automatic safety denial.
#[test]
fn suppressed_proposal_forces_runtime_safety_denial() {
    let service = ModelCallExecutionService::new(
        FixedIds::baseline(),
        FakePrepare {
            outcomes: VecDeque::new(),
            calls: 0,
        },
        UnusedFailure,
        UnusedAuthorization,
        UnusedObservation,
        UnusedProvider,
        InProcessAttemptDispatchGate::default(),
        None,
    );
    let response = signalbox_domain::ToolUsingAssistantResponse::try_from_parts(vec![
        signalbox_domain::AssistantResponsePart::ToolCall(
            signalbox_domain::ToolCallProposal::suppressed(
                signalbox_domain::ToolName::try_new(String::from("sandboxed_exec"))
                    .expect("fixture tool name is valid"),
            ),
        ),
    ])
    .expect("suppressed proposal remains one bounded logical request");
    let observation = ModelCallTerminalObservation::CompletedWithTools {
        response,
        retained_input_tokens: None,
        retained_output_tokens: None,
    };

    assert_eq!(
        service
            .tool_approvals(
                &observation,
                DangerousToolAutoApproval::ApproveAll,
                &[],
                &[]
            )
            .as_ref(),
        [InitialToolApproval::RuntimeSafetyDeny]
    );
}

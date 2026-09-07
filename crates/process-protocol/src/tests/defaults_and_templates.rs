//! Defaults and templates protocol tests.

use super::support::*;
use crate::*;

#[test]
fn adds_forward_only_defaults_replacement() -> Result<(), Box<dyn std::error::Error>> {
    let request_id = request(6)?;
    let request_value = ClientRequest::ReplaceSessionDefaults {
        command_id: command(1)?,
        session_id: uuid(2),
        expected_defaults_version: CanonicalU64::new(3),
        model_selection: ModelSelection::Direct {
            selection_id: uuid(4),
        },
        model_settings: ModelSettingsOverlay::inherit_all(),
        dangerous_tool_auto_approval: true,
        system_prompt: SystemPromptMember::present(None),
    };
    let frame = ClientFrame::try_new_for_version(ProtocolVersion::One, request_id, request_value)?;
    let encoded = encode_client_line(&frame)?;
    assert_eq!(
        String::from_utf8(encoded.clone())?,
        "{\"version\":1,\"request_id\":\"6\",\"request\":{\"type\":\"replace_session_defaults\",\
         \"command_id\":\"00000000-0000-0000-0000-000000000001\",\
         \"session_id\":\"00000000-0000-0000-0000-000000000002\",\
         \"expected_defaults_version\":\"3\",\"model_selection\":{\"kind\":\"direct\",\
         \"selection_id\":\"00000000-0000-0000-0000-000000000004\"},\
         \"model_settings\":{\"reasoning_level\":{\"kind\":\"inherit\"},\
         \"fast_mode\":{\"kind\":\"inherit\"},\"service_tier\":{\"kind\":\"inherit\"}},\
         \"dangerous_tool_auto_approval\":true,\"system_prompt\":null}}\n"
    );
    assert_eq!(decode_client_line(&encoded)?, frame);

    let replacement_receipt = ServerMessage::SessionDefaultsReplaced {
        session_id: uuid(2),
        defaults_version: CanonicalU64::new(4),
        model_selection: ModelSelection::Direct {
            selection_id: uuid(4),
        },
        model_settings: provider_default_settings_snapshot_fixture(),
        dangerous_tool_auto_approval: true,
        system_prompt: SystemPromptMember::present(None),
    };
    assert_server_message_round_trip(
        request(7)?,
        replacement_receipt,
        &format!(
            "{{\"type\":\"session_defaults_replaced\",\"session_id\":\"00000000-0000-0000-0000-000000000002\",\"defaults_version\":\"4\",\"model_selection\":{{\"kind\":\"direct\",\"selection_id\":\"00000000-0000-0000-0000-000000000004\"}},\"model_settings\":{PROVIDER_DEFAULT_SETTINGS_SNAPSHOT_JSON},\"dangerous_tool_auto_approval\":true,\"system_prompt\":null}}"
        ),
    )?;
    let model_identity_entry = ServerMessage::TranscriptEntry {
        entry_index: CanonicalU64::new(5),
        source_session_id: uuid(2),
        entry_id: uuid(6),
        entry: TranscriptEntry::ModelIdentityChanged {
            turn_id: uuid(7),
            defaults_version: CanonicalU64::new(4),
            selected_model_id: uuid(4),
        },
    };
    assert_server_message_round_trip(
        request(8)?,
        model_identity_entry,
        r#"{"type":"transcript_entry","entry_index":"5","source_session_id":"00000000-0000-0000-0000-000000000002","entry_id":"00000000-0000-0000-0000-000000000006","entry":{"type":"model_identity_changed","turn_id":"00000000-0000-0000-0000-000000000007","defaults_version":"4","selected_model_id":"00000000-0000-0000-0000-000000000004"}}"#,
    )?;
    let exhaustion = ServerMessage::Error {
        code: ErrorCode::Rejected,
        message: String::from("defaults version exhausted"),
        detail: ErrorDetail::rejected(RejectionDetail::DefaultsVersionExhausted {
            session_id: uuid(2),
            current: CanonicalU64::new(u64::MAX),
        }),
    };
    assert_server_message_round_trip(
        request(9)?,
        exhaustion,
        r#"{"type":"error","code":"rejected","message":"defaults version exhausted","detail":{"type":"defaults_version_exhausted","session_id":"00000000-0000-0000-0000-000000000002","current":"18446744073709551615"}}"#,
    )?;
    Ok(())
}

#[test]
fn adds_the_bounded_session_system_prompt() -> Result<(), Box<dyn std::error::Error>> {
    // The system-prompt member is required.
    // Every admitted frame must carry the member explicitly.
    assert_client_malformed(
        r#"{"version":1,"request_id":"3","request":{"type":"create_session","command_id":"00000000-0000-0000-0000-000000000001","initial_model_selection":{"kind":"direct","selection_id":"00000000-0000-0000-0000-000000000004"}}}"#,
    );
    assert_client_malformed(
        r#"{"version":1,"request_id":"4","request":{"type":"replace_session_defaults","command_id":"00000000-0000-0000-0000-000000000001","session_id":"00000000-0000-0000-0000-000000000002","expected_defaults_version":"3","model_selection":{"kind":"direct","selection_id":"00000000-0000-0000-0000-000000000004"},"dangerous_tool_auto_approval":true}}"#,
    );
    // The defaults read version member is required and nullable.
    assert_client_malformed(
        r#"{"version":1,"request_id":"5","request":{"type":"read_session_defaults","session_id":"00000000-0000-0000-0000-000000000002"}}"#,
    );
    // A present prompt is nonempty and rejects U+0000.
    assert_client_malformed(
        r#"{"version":1,"request_id":"6","request":{"type":"create_session","command_id":"00000000-0000-0000-0000-000000000001","initial_model_selection":{"kind":"direct","selection_id":"00000000-0000-0000-0000-000000000004"},"system_prompt":""}}"#,
    );
    assert_client_malformed(
        "{\"version\":1,\"request_id\":\"7\",\"request\":{\"type\":\"create_session\",\"command_id\":\"00000000-0000-0000-0000-000000000001\",\"initial_model_selection\":{\"kind\":\"direct\",\"selection_id\":\"00000000-0000-0000-0000-000000000004\"},\"system_prompt\":\"a\\u0000b\"}}",
    );

    let request_id = request(8)?;
    let create = ClientRequest::CreateSession {
        command_id: command(1)?,
        initial_model_selection: ModelSelection::Direct {
            selection_id: uuid(4),
        },
        model_settings: ModelSettingsOverlay::inherit_all(),
        system_prompt: SystemPromptMember::present(Some(SystemPromptText::try_new(
            "exact prompt text".to_owned(),
        )?)),
        placement: crate::SessionPlacement::Pathless {},
        lifecycle: SessionLifecycleMembers::default(),
    };
    let frame = ClientFrame::try_new_for_version(ProtocolVersion::One, request_id, create)?;
    let encoded = encode_client_line(&frame)?;
    assert_eq!(
        String::from_utf8(encoded.clone())?,
        "{\"version\":1,\"request_id\":\"8\",\"request\":{\"type\":\"create_session\",\
         \"command_id\":\"00000000-0000-0000-0000-000000000001\",\
         \"initial_model_selection\":{\"kind\":\"direct\",\
         \"selection_id\":\"00000000-0000-0000-0000-000000000004\"},\
         \"model_settings\":{\"reasoning_level\":{\"kind\":\"inherit\"},\
         \"fast_mode\":{\"kind\":\"inherit\"},\"service_tier\":{\"kind\":\"inherit\"}},\
         \"system_prompt\":\"exact prompt text\"}}\n"
    );
    assert_eq!(decode_client_line(&encoded)?, frame);

    let promptless_create = ClientRequest::CreateSession {
        command_id: command(1)?,
        initial_model_selection: ModelSelection::Direct {
            selection_id: uuid(4),
        },
        model_settings: ModelSettingsOverlay::inherit_all(),
        system_prompt: SystemPromptMember::present(None),
        placement: crate::SessionPlacement::Pathless {},
        lifecycle: SessionLifecycleMembers::default(),
    };
    let promptless_frame =
        ClientFrame::try_new_for_version(ProtocolVersion::One, request_id, promptless_create)?;
    let promptless_encoded = encode_client_line(&promptless_frame)?;
    assert_eq!(
        String::from_utf8(promptless_encoded.clone())?,
        "{\"version\":1,\"request_id\":\"8\",\"request\":{\"type\":\"create_session\",\
         \"command_id\":\"00000000-0000-0000-0000-000000000001\",\
         \"initial_model_selection\":{\"kind\":\"direct\",\
         \"selection_id\":\"00000000-0000-0000-0000-000000000004\"},\
         \"model_settings\":{\"reasoning_level\":{\"kind\":\"inherit\"},\
         \"fast_mode\":{\"kind\":\"inherit\"},\"service_tier\":{\"kind\":\"inherit\"}},\
         \"system_prompt\":null}}\n"
    );
    assert_eq!(decode_client_line(&promptless_encoded)?, promptless_frame);
    let decoded_null = decode_client_line(&line(
        r#"{"version":1,"request_id":"8","request":{"type":"create_session","command_id":"00000000-0000-0000-0000-000000000001","initial_model_selection":{"kind":"direct","selection_id":"00000000-0000-0000-0000-000000000004"},"model_settings":{"reasoning_level":{"kind":"inherit"},"fast_mode":{"kind":"inherit"},"service_tier":{"kind":"inherit"}},"system_prompt":null}}"#,
    ))?;
    let ClientRequest::CreateSession { system_prompt, .. } = decoded_null.request() else {
        panic!("decoded frame must be a create request");
    };
    assert_eq!(system_prompt.value(), Some(&None));

    let replace = ClientRequest::ReplaceSessionDefaults {
        command_id: command(1)?,
        session_id: uuid(2),
        expected_defaults_version: CanonicalU64::new(3),
        model_selection: ModelSelection::Direct {
            selection_id: uuid(4),
        },
        model_settings: ModelSettingsOverlay::inherit_all(),
        dangerous_tool_auto_approval: false,
        system_prompt: SystemPromptMember::present(Some(SystemPromptText::try_new(
            "exact prompt text".to_owned(),
        )?)),
    };
    let frame = ClientFrame::try_new_for_version(ProtocolVersion::One, request(9)?, replace)?;
    let encoded = encode_client_line(&frame)?;
    assert_eq!(
        String::from_utf8(encoded.clone())?,
        "{\"version\":1,\"request_id\":\"9\",\"request\":{\"type\":\"replace_session_defaults\",\
         \"command_id\":\"00000000-0000-0000-0000-000000000001\",\
         \"session_id\":\"00000000-0000-0000-0000-000000000002\",\
         \"expected_defaults_version\":\"3\",\"model_selection\":{\"kind\":\"direct\",\
         \"selection_id\":\"00000000-0000-0000-0000-000000000004\"},\
         \"model_settings\":{\"reasoning_level\":{\"kind\":\"inherit\"},\
         \"fast_mode\":{\"kind\":\"inherit\"},\"service_tier\":{\"kind\":\"inherit\"}},\
         \"dangerous_tool_auto_approval\":false,\
         \"system_prompt\":\"exact prompt text\"}}\n"
    );
    assert_eq!(decode_client_line(&encoded)?, frame);

    let read_current = ClientRequest::ReadSessionDefaults {
        session_id: uuid(2),
        defaults_version: None,
    };
    let frame = ClientFrame::try_new_for_version(ProtocolVersion::One, request(10)?, read_current)?;
    let encoded = encode_client_line(&frame)?;
    assert_eq!(
        String::from_utf8(encoded.clone())?,
        "{\"version\":1,\"request_id\":\"10\",\"request\":{\"type\":\"read_session_defaults\",\
         \"session_id\":\"00000000-0000-0000-0000-000000000002\",\
         \"defaults_version\":null}}\n"
    );
    assert_eq!(decode_client_line(&encoded)?, frame);

    let read_named = ClientRequest::ReadSessionDefaults {
        session_id: uuid(2),
        defaults_version: Some(CanonicalU64::new(3)),
    };
    let frame = ClientFrame::try_new_for_version(ProtocolVersion::One, request(11)?, read_named)?;
    let encoded = encode_client_line(&frame)?;
    assert_eq!(
        String::from_utf8(encoded.clone())?,
        "{\"version\":1,\"request_id\":\"11\",\"request\":{\"type\":\"read_session_defaults\",\
         \"session_id\":\"00000000-0000-0000-0000-000000000002\",\
         \"defaults_version\":\"3\"}}\n"
    );
    assert_eq!(decode_client_line(&encoded)?, frame);

    let receipt = ServerMessage::SessionDefaultsReplaced {
        session_id: uuid(2),
        defaults_version: CanonicalU64::new(4),
        model_selection: ModelSelection::Direct {
            selection_id: uuid(4),
        },
        model_settings: provider_default_settings_snapshot_fixture(),
        dangerous_tool_auto_approval: true,
        system_prompt: SystemPromptMember::present(Some(SystemPromptText::try_new(
            "exact prompt text".to_owned(),
        )?)),
    };
    let frame = ServerFrame::try_new_for_version(ProtocolVersion::One, request(12)?, receipt)?;
    let encoded = encode_server_line(&frame)?;
    assert_eq!(
        String::from_utf8(encoded.clone())?,
        "{\"version\":1,\"request_id\":\"12\",\"message\":{\"type\":\"session_defaults_replaced\",\
         \"session_id\":\"00000000-0000-0000-0000-000000000002\",\
         \"defaults_version\":\"4\",\"model_selection\":{\"kind\":\"direct\",\
         \"selection_id\":\"00000000-0000-0000-0000-000000000004\"},\
         \"model_settings\":"
            .to_owned()
            + PROVIDER_DEFAULT_SETTINGS_SNAPSHOT_JSON
            + ",\
         \"dangerous_tool_auto_approval\":true,\
         \"system_prompt\":\"exact prompt text\"}}\n"
    );
    assert_eq!(decode_server_line(&encoded)?, frame);
    assert_server_malformed(
        r#"{"version":1,"request_id":"12","message":{"type":"session_defaults_replaced","session_id":"00000000-0000-0000-0000-000000000002","defaults_version":"4","model_selection":{"kind":"direct","selection_id":"00000000-0000-0000-0000-000000000004"},"dangerous_tool_auto_approval":true}}"#,
    );

    let defaults_read = ServerMessage::SessionDefaults {
        session_id: uuid(2),
        defaults_version: CanonicalU64::new(4),
        model_selection: ModelSelection::Direct {
            selection_id: uuid(4),
        },
        model_settings: provider_default_settings_snapshot_fixture(),
        dangerous_tool_auto_approval: false,
        system_prompt: Some(SystemPromptText::try_new("exact prompt text".to_owned())?),
    };
    assert_server_message_round_trip(
        request(13)?,
        defaults_read,
        &format!(
            "{{\"type\":\"session_defaults\",\"session_id\":\"00000000-0000-0000-0000-000000000002\",\"defaults_version\":\"4\",\"model_selection\":{{\"kind\":\"direct\",\"selection_id\":\"00000000-0000-0000-0000-000000000004\"}},\"model_settings\":{PROVIDER_DEFAULT_SETTINGS_SNAPSHOT_JSON},\"dangerous_tool_auto_approval\":false,\"system_prompt\":\"exact prompt text\"}}"
        ),
    )?;
    assert_server_message_round_trip(
        request(14)?,
        ServerMessage::SessionDefaults {
            session_id: uuid(2),
            defaults_version: CanonicalU64::new(1),
            model_selection: ModelSelection::Direct {
                selection_id: uuid(4),
            },
            model_settings: provider_default_settings_snapshot_fixture(),
            dangerous_tool_auto_approval: false,
            system_prompt: None,
        },
        &format!(
            "{{\"type\":\"session_defaults\",\"session_id\":\"00000000-0000-0000-0000-000000000002\",\"defaults_version\":\"1\",\"model_selection\":{{\"kind\":\"direct\",\"selection_id\":\"00000000-0000-0000-0000-000000000004\"}},\"model_settings\":{PROVIDER_DEFAULT_SETTINGS_SNAPSHOT_JSON},\"dangerous_tool_auto_approval\":false,\"system_prompt\":null}}"
        ),
    )?;
    Ok(())
}

/// prompt text enforces structural content rules only.
#[test]
fn system_prompt_text_rejects_empty_and_nul_content() {
    let admitted = SystemPromptText::try_new(String::from("exact √ prompt"))
        .expect("structurally valid text is admitted");
    assert_eq!(admitted.as_str(), "exact √ prompt");
    assert!(SystemPromptText::try_new(String::new()).is_err());
    assert!(SystemPromptText::try_new("a\u{0}b".to_owned()).is_err());
}

/// deployment limits use one closed required nullable wire shape.
#[test]
fn deployment_limits_have_exact_closed_wire_shapes() -> Result<(), Box<dyn std::error::Error>> {
    assert_client_request_round_trip(
        request(1)?,
        ClientRequest::ReadDeploymentLimits {},
        r#"{"type":"read_deployment_limits"}"#,
    )?;
    assert_server_message_round_trip(
        request(2)?,
        ServerMessage::DeploymentLimits {
            max_message_utf8_bytes: Some(CanonicalU64::new(7)),
            max_system_prompt_utf8_bytes: None,
            terminal_input_channel_capacity: Some(CanonicalU64::new(3)),
            min_metadata_page_size: Some(CanonicalU64::new(1)),
            max_metadata_page_size: None,
            max_review_findings_per_run: Some(CanonicalU64::new(9)),
        },
        r#"{"type":"deployment_limits","max_message_utf8_bytes":"7","max_system_prompt_utf8_bytes":null,"terminal_input_channel_capacity":"3","min_metadata_page_size":"1","max_metadata_page_size":null,"max_review_findings_per_run":"9"}"#,
    )?;
    Ok(())
}

/// template frames have exact closed shapes.
#[test]
fn template_frames_have_exact_closed_shapes() -> Result<(), Box<dyn std::error::Error>> {
    let create = ClientFrame::try_new_for_version(
        ProtocolVersion::One,
        request(1)?,
        ClientRequest::CreateSessionFromTemplate {
            command_id: command(2)?,
            template_name: "reviewer".to_owned(),
            placement: crate::SessionPlacement::Pathless {},
            lifecycle: SessionLifecycleMembers::default(),
        },
    )?;
    let encoded_create = encode_client_line(&create)?;
    assert_eq!(
        String::from_utf8(encoded_create.clone())?,
        r#"{"version":1,"request_id":"1","request":{"type":"create_session_from_template","command_id":"00000000-0000-0000-0000-000000000002","template_name":"reviewer"}}
"#
    );
    assert_eq!(decode_client_line(&encoded_create)?, create);

    let list = ClientFrame::try_new_for_version(
        ProtocolVersion::One,
        request(2)?,
        ClientRequest::ListTemplates {},
    )?;
    let encoded_list = encode_client_line(&list)?;
    assert_eq!(
        String::from_utf8(encoded_list.clone())?,
        r#"{"version":1,"request_id":"2","request":{"type":"list_templates"}}
"#
    );
    assert_eq!(decode_client_line(&encoded_list)?, list);

    assert_server_message_round_trip(
        request(3)?,
        ServerMessage::TemplatesStart {},
        r#"{"type":"templates_start"}"#,
    )?;
    assert_server_message_round_trip(
        request(3)?,
        ServerMessage::TemplateSummary {
            name: "reviewer".to_owned(),
            version: CanonicalU64::new(7),
        },
        r#"{"type":"template_summary","name":"reviewer","version":"7"}"#,
    )?;
    assert_server_message_round_trip(
        request(3)?,
        ServerMessage::TemplatesEnd {
            template_count: CanonicalU64::new(1),
        },
        r#"{"type":"templates_end","template_count":"1"}"#,
    )?;
    Ok(())
}

#[test]
fn root_placement_creation_and_update_frames_record_global_read_intent_loudly()
-> Result<(), Box<dyn std::error::Error>> {
    let root_path = "operator";
    let root = crate::SessionPlacement::try_root_global_read(String::from(root_path))?;
    let create = ClientFrame::try_new_for_version(
        ProtocolVersion::One,
        request(70)?,
        ClientRequest::CreateSession {
            command_id: command(71)?,
            initial_model_selection: ModelSelection::Direct {
                selection_id: uuid(72),
            },
            model_settings: ModelSettingsOverlay::inherit_all(),
            system_prompt: SystemPromptMember::present(None),
            placement: root.clone(),
            lifecycle: SessionLifecycleMembers::default(),
        },
    )?;
    assert_eq!(
        String::from_utf8(encode_client_line(&create)?)?,
        r#"{"version":1,"request_id":"70","request":{"type":"create_session","command_id":"00000000-0000-0000-0000-000000000047","initial_model_selection":{"kind":"direct","selection_id":"00000000-0000-0000-0000-000000000048"},"model_settings":{"reasoning_level":{"kind":"inherit"},"fast_mode":{"kind":"inherit"},"service_tier":{"kind":"inherit"}},"system_prompt":null,"placement":{"kind":"root_global_read","path":"operator","intent":"acknowledged"}}}
"#
    );
    let update = ClientFrame::try_new_for_version(
        ProtocolVersion::One,
        request(73)?,
        ClientRequest::UpdateSessionPlacement {
            command_id: command(74)?,
            session_id: uuid(75),
            expected_placement_version: CanonicalU64::new(1),
            replacement: root,
        },
    )?;
    assert_eq!(decode_client_line(&encode_client_line(&update)?)?, update);
    assert_eq!(
        crate::SessionPlacement::try_scoped(String::from(root_path)),
        Err(crate::CanonicalValueError::Placement)
    );
    Ok(())
}

#[test]
fn session_placement_constructor_rejects_paths_over_the_structural_byte_bound() {
    let maximum_structural_path = vec!["x".repeat(64); 64].join(".");
    let frame_sized_empty_segments = ".".repeat(crate::MAX_FRAME_BYTES - 1);

    assert!(crate::SessionPlacement::try_scoped(maximum_structural_path).is_ok());
    assert_eq!(
        crate::SessionPlacement::try_scoped(frame_sized_empty_segments),
        Err(crate::CanonicalValueError::Placement)
    );
}

#[test]
fn session_placement_frames_admit_the_complete_structural_range() {
    let maximum_structural_path = vec!["x".repeat(64); 64].join(".");
    let frame = format!(
        r#"{{"version":1,"request_id":"1","request":{{"type":"create_session","command_id":"00000000-0000-0000-0000-000000000047","initial_model_selection":{{"kind":"direct","selection_id":"00000000-0000-0000-0000-000000000048"}},"model_settings":{{"reasoning_level":{{"kind":"inherit"}},"fast_mode":{{"kind":"inherit"}},"service_tier":{{"kind":"inherit"}}}},"system_prompt":null,"placement":{{"kind":"scoped","path":"{maximum_structural_path}"}}}}}}
"#
    );

    decode_client_line(frame.as_bytes()).expect("complete structural path is request-admitted");
    let response = ServerFrame::try_new(
        request(2).expect("fixture request identity is admitted"),
        ServerMessage::SessionSummary {
            session_id: uuid(3),
            defaults_version: CanonicalU64::new(1),
            model_selection: ModelSelection::Alias { alias_id: uuid(4) },
            placement_version: CanonicalU64::new(1),
            placement: crate::SessionPlacement::Scoped {
                path: maximum_structural_path,
            },
            runner: None,
        },
    )
    .expect("legacy structural placement remains response-encodable");
    let encoded = encode_server_line(&response).expect("response encoding succeeds");
    assert_eq!(
        decode_server_line(&encoded).expect("response decoding succeeds"),
        response
    );
}

#[test]
fn session_placement_rejection_versions_are_coherent() {
    assert_placement_version_mismatch_rejected(0, 2);
    assert_placement_version_mismatch_rejected(1, 0);
    assert_placement_version_mismatch_rejected(2, 2);
    assert_eq!(
        placement_version_exhaustion_frame(1)
            .expect_err("nonmaximum placement version cannot be exhausted"),
        FrameValidationError::ErrorDetailShape
    );
    assert!(placement_version_exhaustion_frame(u64::MAX).is_ok());

    let valid = ServerFrame::try_new(
        request(1).expect("fixture request identity is admitted"),
        ServerMessage::Error {
            code: ErrorCode::Rejected,
            message: String::from("placement version mismatch"),
            detail: ErrorDetail::rejected(
                RejectionDetail::SessionPlacementCurrentVersionMismatch {
                    session_id: uuid(2),
                    expected_placement_version: CanonicalU64::new(1),
                    current_placement_version: CanonicalU64::new(2),
                },
            ),
        },
    );
    assert!(valid.is_ok());
}

/// invalid template names or versions cannot enter admitted frames.
#[test]
fn template_frames_require_valid_values() -> Result<(), Box<dyn std::error::Error>> {
    assert_client_malformed(
        r#"{"version":1,"request_id":"1","request":{"type":"create_session_from_template","command_id":"00000000-0000-0000-0000-000000000002","template_name":"Reviewer"}}"#,
    );
    assert_eq!(
        ServerFrame::try_new_for_version(
            ProtocolVersion::One,
            request(1)?,
            ServerMessage::TemplateSummary {
                name: "reviewer".to_owned(),
                version: CanonicalU64::new(0),
            },
        )
        .expect_err("zero template version is rejected"),
        FrameValidationError::TemplateShape
    );
    Ok(())
}

/// a frame at the single version is admitted unchanged.
#[test]
fn single_protocol_version_is_admitted() -> Result<(), Box<dyn std::error::Error>> {
    let frame = ClientFrame::try_new(request(1)?, ClientRequest::ListSessions {})?;
    let encoded = encode_client_line(&frame)?;

    assert_eq!(frame.version(), ProtocolVersion::One);
    assert_eq!(decode_client_line(&encoded)?, frame);
    Ok(())
}

/// the integer immediately below the single version is refused.
#[test]
fn version_below_single_version_is_refused() {
    assert_unsupported_version("0");
}

/// closed-enum decoding refuses an unknown version member.
#[test]
fn unknown_protocol_version_member_is_refused() {
    let error = serde_json::from_str::<ProtocolVersion>("2")
        .expect_err("an unknown protocol version must be refused");

    assert!(error.to_string().contains("frame version is unsupported"));
}

/// the model-alias catalog has exact closed shapes.
#[test]
fn model_alias_catalog_has_exact_closed_shapes() -> Result<(), Box<dyn std::error::Error>> {
    let request_id = request(1)?;
    let frame = ClientFrame::try_new_for_version(
        ProtocolVersion::One,
        request_id,
        ClientRequest::ListModelAliases {},
    )?;
    let encoded = encode_client_line(&frame)?;
    assert_eq!(
        String::from_utf8(encoded.clone())?,
        "{\"version\":1,\"request_id\":\"1\",\"request\":{\"type\":\"list_model_aliases\"}}\n"
    );
    assert_eq!(decode_client_line(&encoded)?, frame);
    assert_server_message_round_trip(
        request(2)?,
        ServerMessage::ModelAliasesStart {},
        r#"{"type":"model_aliases_start"}"#,
    )?;
    assert_server_message_round_trip(
        request(3)?,
        ServerMessage::ModelAliasSummary {
            alias_id: uuid(4),
            selection_id: uuid(5),
        },
        r#"{"type":"model_alias_summary","alias_id":"00000000-0000-0000-0000-000000000004","selection_id":"00000000-0000-0000-0000-000000000005"}"#,
    )?;
    assert_server_message_round_trip(
        request(6)?,
        ServerMessage::ModelAliasesEnd {
            alias_count: CanonicalU64::new(1),
        },
        r#"{"type":"model_aliases_end","alias_count":"1"}"#,
    )?;
    Ok(())
}

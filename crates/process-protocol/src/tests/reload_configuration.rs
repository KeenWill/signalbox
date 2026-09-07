//! Reload request correlation, closed receipts, and strict frame admission.

use super::support::*;
use crate::*;

#[test]
fn reload_request_and_terminal_receipts_round_trip() -> Result<(), Box<dyn std::error::Error>> {
    let id = command(1)?;
    let frame = ClientFrame::try_new_for_version(
        ProtocolVersion::One,
        request(1)?,
        ClientRequest::ReloadConfiguration { command_id: id },
    )?;
    let encoded = encode_client_line(&frame)?;
    assert_eq!(decode_client_line(&encoded)?, frame);
    for message in [
        ServerMessage::ConfigurationReloaded {
            command_id: id,
            reloaded_sections: ReloadedSection::ALL.to_vec(),
        },
        ServerMessage::ConfigurationReloadFailed {
            command_id: id,
            phase: ConfigurationReloadPhase::Validate,
            reason: "startup-only configuration differs".to_owned(),
        },
    ] {
        let frame = ServerFrame::try_new_for_version(ProtocolVersion::One, request(1)?, message)?;
        assert_eq!(decode_server_line(&encode_server_line(&frame)?)?, frame);
    }
    Ok(())
}

#[test]
fn reload_rejects_extra_request_fields_and_unknown_receipt_sections() {
    assert_client_malformed(
        r#"{"version":1,"request_id":"1","request":{"type":"reload_configuration","command_id":"00000000-0000-0000-0000-000000000001","path":"/untrusted"}}"#,
    );
    assert_server_malformed(
        r#"{"version":1,"request_id":"1","message":{"type":"configuration_reloaded","command_id":"00000000-0000-0000-0000-000000000001","reloaded_sections":["credentials"]}}"#,
    );
    assert_server_malformed(
        r#"{"version":1,"request_id":"1","message":{"type":"configuration_reload_failed","command_id":"00000000-0000-0000-0000-000000000001","phase":"future_phase","reason":"refused"}}"#,
    );
}

#[test]
fn reload_receipts_reject_control_text_and_oversized_reasons()
-> Result<(), Box<dyn std::error::Error>> {
    for reason in [
        String::new(),
        "line\nbreak".to_owned(),
        "x".repeat(MAX_CONFIGURATION_RELOAD_REASON_BYTES + 1),
    ] {
        assert!(
            ServerFrame::try_new_for_version(
                ProtocolVersion::One,
                request(1)?,
                ServerMessage::ConfigurationReloadFailed {
                    command_id: command(1)?,
                    phase: ConfigurationReloadPhase::Read,
                    reason,
                }
            )
            .is_err()
        );
    }
    Ok(())
}

#[test]
fn reload_success_requires_the_complete_ordered_section_inventory() {
    for inventory in [
        serde_json::json!([]),
        serde_json::json!(["model_catalog", "session_templates"]),
        serde_json::json!([
            "model_catalog",
            "session_templates",
            "repo_watch",
            "repo_watch"
        ]),
        serde_json::json!(["repo_watch", "session_templates", "model_catalog"]),
    ] {
        let frame = serde_json::json!({
            "version":1, "request_id":"1", "message": {
                "type":"configuration_reloaded",
                "command_id":"00000000-0000-0000-0000-000000000001",
                "reloaded_sections":inventory,
            },
        });
        assert_server_malformed(&frame.to_string());
    }
}

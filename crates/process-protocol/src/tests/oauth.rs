//! OAuth administration wire admission and progress bounds.

use super::support::*;
use crate::*;

#[test]
fn oauth_requests_and_receipts_round_trip_and_reject_extensions()
-> Result<(), Box<dyn std::error::Error>> {
    // The numeric identities are arbitrary; correlation must be preserved.
    let id = command(1)?;
    for (kind, value) in [
        (
            "provision_oauth_credential",
            ClientRequest::ProvisionOauthCredential {
                command_id: id,
                profile: "subscription".into(),
            },
        ),
        (
            "reprovision_oauth_credential",
            ClientRequest::ReprovisionOauthCredential {
                command_id: id,
                profile: "subscription".into(),
            },
        ),
        (
            "delete_oauth_credential",
            ClientRequest::DeleteOauthCredential {
                command_id: id,
                profile: "subscription".into(),
            },
        ),
    ] {
        let json = serde_json::json!({"type": kind, "command_id": id, "profile": "subscription"});
        assert_eq!(serde_json::to_value(&value)?, json);
        assert_eq!(
            serde_json::from_value::<ClientRequest>(json.clone())?,
            value
        );
        let mut extra = json;
        extra["access_token"] = "unadmitted".into();
        assert!(serde_json::from_value::<ClientRequest>(extra).is_err());
    }
    let message = ServerMessage::OauthCredentialReceipt {
        command_id: id,
        profile: "subscription".into(),
        outcome: OauthCredentialOutcome::Failed {
            reason: OauthCredentialFailure::UnknownProfile,
        },
    };
    assert_server_message_round_trip(
        request(1)?,
        message,
        r#"{"type":"oauth_credential_receipt","command_id":"00000000-0000-0000-0000-000000000001","profile":"subscription","outcome":{"kind":"failed","reason":"unknown_profile"}}"#,
    )?;
    for outcome in [
        serde_json::json!({"kind": "failed", "reason": "new_failure"}),
        serde_json::json!({"kind": "failed"}),
        serde_json::json!({"kind": "provisioned", "reason": "unknown_profile"}),
        serde_json::json!({"kind": "new_outcome"}),
    ] {
        assert!(serde_json::from_value::<OauthCredentialOutcome>(outcome).is_err());
    }
    Ok(())
}

#[test]
fn oauth_progress_enforces_printable_code_and_https_uri_bounds() {
    let valid_uri = "https://authorization.example/verify";
    assert!(validate_oauth_authorization(&"A".repeat(256), valid_uri).is_ok());
    for code in [
        String::new(),
        "A".repeat(257),
        "line\nfeed".into(),
        "tab\t".into(),
        "nonascii-é".into(),
        "\u{7f}".into(),
    ] {
        assert_eq!(
            validate_oauth_authorization(&code, valid_uri),
            Err(FrameValidationError::OauthCredentialShape)
        );
    }
    for uri in [
        "http://authorization.example",
        "/verify",
        "https:/authorization.example",
        "https:///authorization.example",
        "https://user@authorization.example",
        "https://@authorization.example",
        "https://authorization.example/#fragment",
        "https://authorization.example/\nverify",
    ] {
        assert_eq!(
            validate_oauth_authorization("CODE", uri),
            Err(FrameValidationError::OauthCredentialShape),
            "{uri:?}"
        );
    }
    let prefix = "https://authorization.example/";
    let uri = format!("{prefix}{}", "a".repeat(4096 - prefix.len()));
    assert!(validate_oauth_authorization("CODE", &uri).is_ok());
    assert!(validate_oauth_authorization("CODE", &(uri + "a")).is_err());
}

#[test]
fn oauth_profile_names_follow_catalog_bounds() -> Result<(), Box<dyn std::error::Error>> {
    for profile in [
        String::new(),
        "a".repeat(257),
        " padded".into(),
        "padded ".into(),
        "nul\0".into(),
    ] {
        assert_eq!(
            ClientRequest::DeleteOauthCredential {
                command_id: command(1)?,
                profile
            }
            .validate(),
            Err(FrameValidationError::OauthCredentialShape)
        );
    }
    assert!(
        ClientRequest::DeleteOauthCredential {
            command_id: command(1)?,
            profile: "a".repeat(256)
        }
        .validate()
        .is_ok()
    );
    Ok(())
}

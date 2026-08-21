//! Runner wire contract tests.

use serde_json::json;
use uuid::Uuid;

use super::*;

const ARBITRARY_UUID_A: &str = "00000000-0000-4000-8000-000000000001";
const ARBITRARY_UUID_B: &str = "00000000-0000-4000-8000-000000000002";
const ARBITRARY_UUID_C: &str = "00000000-0000-4000-8000-000000000003";
const ARBITRARY_UUID_D: &str = "00000000-0000-4000-8000-000000000004";
const ARBITRARY_UUID_E: &str = "00000000-0000-4000-8000-000000000005";
const ARBITRARY_UUID_F: &str = "00000000-0000-4000-8000-000000000006";
const ARBITRARY_UUID_G: &str = "00000000-0000-4000-8000-000000000007";
const ARBITRARY_UUID_H: &str = "00000000-0000-4000-8000-000000000008";
const EXPECTED_ADVERTISEMENT_DIGEST: &str =
    "d2cfb8a873b962f27dab0882992b14e194bf441c7002e00a237d6ed0f32fd187";
const EXPECTED_CLONE_URL_DIGEST: &str =
    "1a65f9f5977dc0dcfaae9165099f5639eaa3562991fa3242153f363c868ce930";
const EXPECTED_MANIFEST_DIGEST: &str =
    "3eb9e28c4ff2c0bc069a3064a8eebe4a1ab8b1169bb3c8b3ed388ba7d232e3ef";
const EXPECTED_UNBORN_MANIFEST_DIGEST: &str =
    "654ee9e8b849a80ed819214c5188615d521a229e28802fd3a6271512e280e374";

fn uuid(value: &str) -> CanonicalUuid {
    CanonicalUuid::from_uuid(
        Uuid::parse_str(value).unwrap_or_else(|error| panic!("synthetic UUID is valid: {error}")),
    )
}

fn positive(value: u64) -> PositiveU64 {
    PositiveU64::try_new(value)
        .unwrap_or_else(|error| panic!("positive fixture value is valid: {error}"))
}

fn digest(value: &str) -> Digest {
    Digest::try_new(value.to_owned())
        .unwrap_or_else(|error| panic!("digest fixture is valid: {error}"))
}

fn tool(value: &str) -> WireToolName {
    WireToolName::try_new(value.to_owned())
        .unwrap_or_else(|error| panic!("tool fixture is valid: {error}"))
}

fn detail_name(value: &str) -> DetailName {
    DetailName::try_new(value.to_owned())
        .unwrap_or_else(|error| panic!("detail-name fixture is valid: {error}"))
}

fn profile(value: &str) -> ProfileName {
    ProfileName::try_new(value.to_owned())
        .unwrap_or_else(|error| panic!("profile fixture is valid: {error}"))
}

fn repository(value: &str) -> RepositoryKey {
    RepositoryKey::try_new(value.to_owned())
        .unwrap_or_else(|error| panic!("repository fixture is valid: {error}"))
}

fn working_directory(value: &str) -> WorkingDirectory {
    WorkingDirectory::try_new(value.to_owned())
        .unwrap_or_else(|error| panic!("working-directory fixture is valid: {error}"))
}

fn capability(value: &str) -> CapabilityName {
    CapabilityName::try_new(value.to_owned())
        .unwrap_or_else(|error| panic!("capability fixture is valid: {error}"))
}

fn lease_correlation() -> LeaseCorrelation {
    LeaseCorrelation {
        registration_revision: positive(7),
        lease_id: uuid(ARBITRARY_UUID_A),
        lease_generation: positive(2),
        runner_id: uuid(ARBITRARY_UUID_B),
        placement_revision: positive(3),
        working_directory: working_directory("sessions/example/3/repo"),
        sandbox_profile: SandboxProfile::WorkspaceRestricted,
        tool_name: tool("git_fetch"),
        session_id: uuid(ARBITRARY_UUID_C),
        turn_id: uuid(ARBITRARY_UUID_D),
        tool_request_id: uuid(ARBITRARY_UUID_E),
        tool_attempt_id: uuid(ARBITRARY_UUID_F),
        issuing_turn_attempt_id: uuid(ARBITRARY_UUID_G),
        tool_dispatch_generation: positive(1),
    }
}

fn provision_correlation() -> ProvisionCorrelation {
    ProvisionCorrelation {
        authorization_id: uuid(ARBITRARY_UUID_A),
        session_id: uuid(ARBITRARY_UUID_B),
        placement_revision: positive(3),
        runner_id: uuid(ARBITRARY_UUID_C),
        registration_revision: positive(7),
        repository: Some(repository("primary")),
        sandbox_profile: SandboxProfile::WorkspaceRestricted,
        credential_profile: Some(profile("code_host")),
    }
}

fn release_correlation() -> ReleaseCorrelation {
    ReleaseCorrelation {
        session_id: uuid(ARBITRARY_UUID_B),
        placement_revision: positive(3),
        runner_id: uuid(ARBITRARY_UUID_C),
        manifest_id: uuid(ARBITRARY_UUID_H),
    }
}

fn advertisement() -> Advertisement {
    Advertisement {
        capability_classes: vec![capability("workstation")],
        tools: vec![tool("git_fetch")],
        workspace_capabilities: vec![WorkspaceCapability::WorktreePerSession],
        sandbox_profiles: vec![SandboxProfile::Ambient, SandboxProfile::WorkspaceRestricted],
        credential_profiles: vec![profile("code_host")],
        repositories: vec![RepositoryEntry {
            key: repository("primary"),
            credential_profile: Some(profile("code_host")),
        }],
    }
}

fn manifest() -> WorkspaceManifest {
    WorkspaceManifest {
        lifecycle: ManifestLifecycle::Ready,
        manifest_id: uuid(ARBITRARY_UUID_H),
        session: uuid(ARBITRARY_UUID_B),
        placement_revision: positive(3),
        runner: uuid(ARBITRARY_UUID_C),
        repository: Some(repository("primary")),
        canonical_clone_url_digest: Some(digest(EXPECTED_CLONE_URL_DIGEST)),
        credential_profile: Some(profile("code_host")),
        sandbox_profile: SandboxProfile::WorkspaceRestricted,
        relative_path: "sessions/00000000-0000-4000-8000-000000000002/3/repo".to_owned(),
        recovery: Some(Recovery::Branch {
            name: "main".to_owned(),
            revision: "0123456789abcdef0123456789abcdef01234567".to_owned(),
        }),
    }
}

fn frame(message: Message) -> Frame {
    Frame::try_new(message).unwrap_or_else(|error| panic!("wire fixture must be valid: {error}"))
}

fn heartbeat_frame() -> Frame {
    frame(Message::Heartbeat(Heartbeat {
        sequence: positive(9),
        last_accepted_peer_sequence: 8,
    }))
}

fn leak_page_frame() -> Frame {
    let report_digest = digest(EXPECTED_ADVERTISEMENT_DIGEST);
    let registration_revision = positive(7);
    let page = positive(1);
    let facts = Vec::new();
    let page_digest = leak_page_digest(LeakPageDigestInput {
        registration_revision,
        report_digest: &report_digest,
        page,
        prior_page_digest: None,
        final_page: true,
        facts: &facts,
    })
    .unwrap_or_else(|error| panic!("page-one leak digest is valid: {error}"));
    frame(Message::WorkspaceLeakPage(WorkspaceLeakPage {
        page: LeakPage {
            correlation: LeakPageCorrelation {
                registration_revision,
                report_digest,
                page,
            },
            prior_page_digest: None,
            final_page: true,
            facts,
            page_digest,
        },
    }))
}

fn dispatch_frame(padding_bytes: usize) -> Frame {
    frame(Message::Dispatch(Dispatch {
        correlation: lease_correlation(),
        normalized_arguments: json!({ "padding": "x".repeat(padding_bytes) }),
    }))
}

fn dispatch_at_complete_line_bytes(bytes: usize) -> Frame {
    let empty = encode_line(&dispatch_frame(0))
        .unwrap_or_else(|error| panic!("empty dispatch encodes: {error}"));
    dispatch_frame(bytes - empty.len())
}

fn detail(value: serde_json::Value) -> FailureDetail {
    FailureDetail::try_new(
        detail_name("runner.failure"),
        "synthetic failure".to_owned(),
        value,
    )
    .unwrap_or_else(|error| panic!("failure detail fixture must be valid: {error}"))
}

fn deeply_nested_detail() -> serde_json::Value {
    json!({"a":{"a":{"a":{"a":{"a":{"a":{"a":{"a":{}}}}}}}}})
}

#[test]
fn frame_round_trip_preserves_closed_message() {
    let expected = heartbeat_frame();
    let encoded =
        encode_line(&expected).unwrap_or_else(|error| panic!("heartbeat frame encodes: {error}"));
    let actual =
        decode_line(&encoded).unwrap_or_else(|error| panic!("heartbeat frame decodes: {error}"));

    assert_eq!(actual, expected);
}

#[test]
fn frame_round_trip_preserves_unborn_workspace_recovery() {
    let mut ready_manifest = manifest();
    ready_manifest.recovery = Some(Recovery::UnbornBranch {
        name: "main".to_owned(),
    });
    let ready_digest = workspace_manifest_digest(&ready_manifest)
        .unwrap_or_else(|error| panic!("unborn manifest digests: {error}"));
    let expected = frame(Message::WorkspaceReady(WorkspaceReady {
        correlation: provision_correlation(),
        ready: ReadyManifest::try_new(
            ready_manifest,
            ready_digest,
            working_directory("/runner/sessions/unborn/repo"),
        )
        .unwrap_or_else(|error| panic!("unborn ready manifest is valid: {error}")),
    }));
    let encoded = encode_line(&expected)
        .unwrap_or_else(|error| panic!("unborn ready frame encodes: {error}"));
    let actual =
        decode_line(&encoded).unwrap_or_else(|error| panic!("unborn ready frame decodes: {error}"));

    assert_eq!(actual, expected);
}

#[test]
fn encoder_admits_exact_eight_mib_complete_line() {
    let exact = dispatch_at_complete_line_bytes(MAX_FRAME_BYTES);
    let encoded =
        encode_line(&exact).unwrap_or_else(|error| panic!("exact-bound frame encodes: {error}"));

    assert_eq!(encoded.len(), MAX_FRAME_BYTES);
}

#[test]
fn encoder_rejects_one_byte_over_eight_mib() {
    let oversized = dispatch_at_complete_line_bytes(MAX_FRAME_BYTES + 1);

    assert!(encode_line(&oversized).is_err());
}

#[test]
fn decoder_admits_exact_eight_mib_complete_line() {
    let exact = dispatch_at_complete_line_bytes(MAX_FRAME_BYTES);
    let encoded =
        encode_line(&exact).unwrap_or_else(|error| panic!("exact-bound frame encodes: {error}"));

    assert!(decode_line(&encoded).is_ok());
}

#[test]
fn decoder_rejects_one_byte_over_eight_mib_before_json() {
    let mut oversized = vec![b' '; MAX_FRAME_BYTES + 1];
    oversized[MAX_FRAME_BYTES] = b'\n';

    assert!(decode_line(&oversized).is_err());
}

#[test]
fn decoder_rejects_missing_newline() {
    assert!(decode_line(br#"{"version":2,"kind":"heartbeat","payload":{"sequence":1,"last_accepted_peer_sequence":0}}"#).is_err());
}

#[test]
fn decoder_rejects_embedded_physical_newline() {
    assert!(decode_line(b"{}\n{}\n").is_err());
}

#[test]
fn decoder_rejects_unknown_top_level_member() {
    assert!(decode_line(br#"{"version":2,"kind":"heartbeat","payload":{"sequence":1,"last_accepted_peer_sequence":0},"extra":false}
"#).is_err());
}

#[test]
fn decoder_rejects_unknown_payload_member() {
    assert!(decode_line(br#"{"version":2,"kind":"heartbeat","payload":{"sequence":1,"last_accepted_peer_sequence":0,"extra":false}}
"#).is_err());
}

#[test]
fn decoder_rejects_unknown_message_kind() {
    assert!(
        decode_line(
            br#"{"version":2,"kind":"future","payload":{}}
"#
        )
        .is_err()
    );
}

#[test]
fn decoder_rejects_unsupported_version() {
    assert!(decode_line(br#"{"version":3,"kind":"heartbeat","payload":{"sequence":1,"last_accepted_peer_sequence":0}}
"#).is_err());
}

#[test]
fn decoder_reports_unsupported_version_before_future_kind() {
    let Err(FrameError::UnsupportedVersion(actual)) = decode_line(
        br#"{"version":3,"kind":"future","payload":{}}
"#,
    ) else {
        panic!("version three must fail before closed-kind decoding");
    };

    assert_eq!(actual, 3);
}

#[test]
fn decoder_rejects_zero_positive_sequence() {
    assert!(decode_line(br#"{"version":2,"kind":"heartbeat","payload":{"sequence":0,"last_accepted_peer_sequence":0}}
"#).is_err());
}

#[test]
fn decoder_rejects_noncanonical_uuid() {
    assert!(decode_line(br#"{"version":2,"kind":"enroll","payload":{"request_id":"00000000-0000-4000-8000-00000000000A","digest_version":1,"advertisement":{"capability_classes":[],"tools":[],"workspace_capabilities":[],"sandbox_profiles":[],"credential_profiles":[],"repositories":[]}}}
"#).is_err());
}

#[test]
fn unsupported_digest_version_passes_structural_validation() {
    let request = Enroll {
        request_id: uuid(ARBITRARY_UUID_A),
        digest_version: DIGEST_VERSION + 1,
        advertisement: advertisement(),
    };
    let expected = Message::Enroll(request.clone());

    let observed = Frame::try_new(Message::Enroll(request))
        .unwrap_or_else(|error| panic!("structural advertisement validation succeeds: {error}"));

    assert_eq!(observed.message, expected);
}

#[test]
fn decoder_rejects_json_null_for_absent_only_phase() {
    assert!(decode_line(br#"{"version":2,"kind":"heartbeat_ack","payload":{"challenge_sequence":1,"runner_sequence":1,"lease_phase":null}}
"#).is_err());
}

#[test]
fn encoder_emits_explicit_null_for_page_one_prior_digest() {
    let encoded = encode_line(&leak_page_frame())
        .unwrap_or_else(|error| panic!("page-one leak frame encodes: {error}"));
    let value: serde_json::Value = serde_json::from_slice(&encoded)
        .unwrap_or_else(|error| panic!("encoded page-one leak frame parses: {error}"));
    let page = value
        .get("payload")
        .and_then(|payload| payload.get("page"))
        .and_then(serde_json::Value::as_object)
        .unwrap_or_else(|| panic!("page-one leak payload contains a page object"));

    assert!(page.contains_key("prior_page_digest"));
    assert!(page["prior_page_digest"].is_null());
}

#[test]
fn decoder_rejects_omitted_page_one_prior_digest() {
    let encoded = encode_line(&leak_page_frame())
        .unwrap_or_else(|error| panic!("page-one leak frame encodes: {error}"));
    let mut value: serde_json::Value = serde_json::from_slice(&encoded)
        .unwrap_or_else(|error| panic!("encoded page-one leak frame parses: {error}"));
    value
        .get_mut("payload")
        .and_then(|payload| payload.get_mut("page"))
        .and_then(serde_json::Value::as_object_mut)
        .unwrap_or_else(|| panic!("page-one leak payload contains a page object"))
        .remove("prior_page_digest");
    let mut omitted = serde_json::to_vec(&value)
        .unwrap_or_else(|error| panic!("omitted page-one leak frame serializes: {error}"));
    omitted.push(0x0a);

    assert!(decode_line(&omitted).is_err());
}

#[test]
fn heartbeat_rejects_lease_offer_as_workspace_failure_correlation() {
    let mut encoded = serde_json::to_vec(&json!({
        "version": 2,
        "kind": "heartbeat_ack",
        "payload": {
            "challenge_sequence": 1,
            "runner_sequence": 1,
            "workspace_phase": {
                "phase": "failure_unrecorded",
                "correlation": {
                    "kind": "lease_offer",
                    "correlation": lease_correlation()
                }
            }
        }
    }))
    .unwrap_or_else(|error| panic!("heartbeat acknowledgement serializes: {error}"));
    encoded.push(b'\n');

    assert!(decode_line(&encoded).is_err());
}

#[test]
fn decoder_rejects_available_correlation_member_from_another_arm() {
    assert!(
        decode_line(
            br#"{"version":2,"kind":"rejected","payload":{"offending_kind":"future","available_correlation":{"kind":"none","extra":false},"code":"malformed_frame"}}
"#
        )
        .is_err()
    );
}

#[test]
fn decoder_rejects_unknown_rejection_code() {
    assert!(
        decode_line(
            br#"{"version":2,"kind":"rejected","payload":{"offending_kind":"future","available_correlation":{"kind":"none"},"code":"future_code"}}
"#
        )
        .is_err()
    );
}

#[test]
fn decoder_rejects_terminal_union_member_from_another_arm() {
    let encoded = format!(
        "{{\"version\":2,\"kind\":\"result\",\"payload\":{{\"correlation\":{},\"result\":{{\"kind\":\"ambiguous\",\"text\":\"no\"}}}}}}\n",
        serde_json::to_string(&lease_correlation())
            .unwrap_or_else(|error| panic!("lease correlation serializes: {error}"))
    );

    assert!(decode_line(encoded.as_bytes()).is_err());
}

#[test]
fn decoder_rejects_duplicate_terminal_result_kind() {
    let encoded = format!(
        "{{\"version\":2,\"kind\":\"result\",\"payload\":{{\"correlation\":{},\"result\":{{\"kind\":\"success\",\"kind\":\"success\",\"text\":\"ok\"}}}}}}\n",
        serde_json::to_string(&lease_correlation())
            .unwrap_or_else(|error| panic!("lease correlation serializes: {error}"))
    );

    assert!(decode_line(encoded.as_bytes()).is_err());
}

#[test]
fn decoder_rejects_duplicate_terminal_result_member() {
    let encoded = format!(
        "{{\"version\":2,\"kind\":\"result\",\"payload\":{{\"correlation\":{},\"result\":{{\"kind\":\"success\",\"text\":\"first\",\"text\":\"second\"}}}}}}\n",
        serde_json::to_string(&lease_correlation())
            .unwrap_or_else(|error| panic!("lease correlation serializes: {error}"))
    );

    assert!(decode_line(encoded.as_bytes()).is_err());
}

#[test]
fn working_directory_rejects_empty_nul_and_oversized_text() {
    assert!(WorkingDirectory::try_new(String::new()).is_err());
    assert!(WorkingDirectory::try_new("workspace\0repo".to_owned()).is_err());
    assert!(WorkingDirectory::try_new("x".repeat(WorkingDirectory::MAX_BYTES + 1)).is_err());
}

#[test]
fn advertisement_rejects_unsorted_inventory() {
    let mut invalid = advertisement();
    invalid.tools = vec![tool("zeta"), tool("alpha")];

    assert!(invalid.validate().is_err());
}

#[test]
fn advertisement_rejects_duplicate_inventory() {
    let mut invalid = advertisement();
    invalid.sandbox_profiles = vec![SandboxProfile::Ambient, SandboxProfile::Ambient];

    assert!(invalid.validate().is_err());
}

#[test]
fn advertisement_rejects_repository_profile_outside_profile_inventory() {
    let mut invalid = advertisement();
    invalid.credential_profiles = Vec::new();

    assert!(invalid.validate().is_err());
}

#[test]
fn lease_offer_rejects_negotiated_result_bounds() {
    let invalid = Message::LeaseOffer(LeaseOffer {
        correlation: lease_correlation(),
        effect_class: EffectClass::Pure,
        credential_profile: None,
        grant_revision: None,
        normalized_arguments: json!({}),
        result_bounds: ResultBounds {
            success_text_bytes: SUCCESS_TEXT_BYTES - 1,
            failure_detail_bytes: FAILURE_DETAIL_BYTES,
        },
    });

    assert!(Frame::try_new(invalid).is_err());
}

#[test]
fn result_rejects_oversized_success_text() {
    let invalid = Message::Result(ResultFrame {
        correlation: lease_correlation(),
        result: TerminalResult::Success {
            text: "x".repeat(SUCCESS_TEXT_BYTES as usize + 1),
        },
    });

    assert!(Frame::try_new(invalid).is_err());
}

#[test]
fn operation_failure_rejects_category_correlation_mismatch() {
    let invalid = Message::OperationFailed(OperationFailed {
        failure: OperationFailure {
            correlation: OperationCorrelation::Release(release_correlation()),
            category: FailureCategory::CredentialUnavailable,
            detail: detail(json!({})),
        },
    });

    assert!(Frame::try_new(invalid).is_err());
}

#[test]
fn failure_detail_rejects_unchecked_member_name() {
    assert!(
        FailureDetail::try_new(
            detail_name("runner.failure"),
            "synthetic failure".to_owned(),
            json!({"bad key": true})
        )
        .is_err()
    );
}

#[test]
fn failure_detail_accepts_catalog_key_grammar_for_code_and_payload() {
    let actual = FailureDetail::try_new(
        detail_name("git.clone"),
        "synthetic failure".to_owned(),
        json!({"git.ref": true}),
    );

    assert!(actual.is_ok());
}

#[test]
fn failure_detail_rejects_ninth_container() {
    assert!(
        FailureDetail::try_new(
            detail_name("runner.failure"),
            "synthetic failure".to_owned(),
            deeply_nested_detail()
        )
        .is_err()
    );
}

#[test]
fn decoder_rejects_failure_payload_over_raw_received_byte_bound() {
    let payload_text = "a".repeat(350);
    let valid = frame(Message::OperationFailed(OperationFailed {
        failure: OperationFailure {
            correlation: OperationCorrelation::Release(release_correlation()),
            category: FailureCategory::WorkspaceCleanupFailed,
            detail: detail(json!({"value": payload_text.clone()})),
        },
    }));
    let encoded = String::from_utf8(
        encode_line(&valid).unwrap_or_else(|error| panic!("failure frame encodes: {error}")),
    )
    .unwrap_or_else(|error| panic!("failure frame is UTF-8: {error}"));
    let compact = serde_json::to_string(&payload_text)
        .unwrap_or_else(|error| panic!("payload text serializes: {error}"));
    let escaped = format!("\"{}\"", "\\u0061".repeat(350));
    let received = encoded.replacen(&compact, &escaped, 1);

    assert!(decode_line(received.as_bytes()).is_err());
}

#[test]
fn manifest_rejects_partial_repository_union() {
    let mut invalid = manifest();
    invalid.recovery = None;

    assert!(workspace_manifest_digest(&invalid).is_err());
}

#[test]
fn manifest_rejects_invalid_branch_ref() {
    let mut invalid = manifest();
    invalid.recovery = Some(Recovery::Branch {
        name: "bad..branch".to_owned(),
        revision: "0123456789abcdef0123456789abcdef01234567".to_owned(),
    });

    assert!(workspace_manifest_digest(&invalid).is_err());
}

#[test]
fn manifest_rejects_abbreviated_revision() {
    let mut invalid = manifest();
    invalid.recovery = Some(Recovery::Commit {
        revision: "0123456".to_owned(),
    });

    assert!(workspace_manifest_digest(&invalid).is_err());
}

#[test]
fn ready_frame_rejects_manifest_digest_disagreement() {
    assert!(
        ReadyManifest::try_new(
            manifest(),
            digest(EXPECTED_ADVERTISEMENT_DIGEST),
            working_directory("/runner/sessions/ready/repo"),
        )
        .is_err()
    );
}

#[test]
fn ready_frame_rejects_nondeterministic_relative_path() {
    let mut ready_manifest = manifest();
    ready_manifest.relative_path =
        "sessions/00000000-0000-4000-8000-000000000002/3/alternate".to_owned();
    let manifest_digest = workspace_manifest_digest(&ready_manifest)
        .unwrap_or_else(|error| panic!("changed manifest digests: {error}"));
    let invalid = Message::WorkspaceReady(WorkspaceReady {
        correlation: provision_correlation(),
        ready: ReadyManifest::try_new(
            ready_manifest,
            manifest_digest,
            working_directory("/runner/sessions/alternate/repo"),
        )
        .unwrap_or_else(|error| panic!("alternate ready manifest is valid: {error}")),
    });

    assert!(Frame::try_new(invalid).is_err());
}

#[test]
fn advertisement_digest_is_deterministic() {
    let fixture = advertisement();
    let first = advertisement_digest(&fixture)
        .unwrap_or_else(|error| panic!("advertisement digests: {error}"));
    let second = advertisement_digest(&fixture)
        .unwrap_or_else(|error| panic!("advertisement digests again: {error}"));

    assert_eq!(first, second);
}

#[test]
fn advertisement_digest_preimage_is_pinned() {
    let actual = advertisement_digest(&advertisement())
        .unwrap_or_else(|error| panic!("advertisement digests: {error}"));

    assert_eq!(actual.as_str(), EXPECTED_ADVERTISEMENT_DIGEST);
}

#[test]
fn clone_url_digest_is_pinned() {
    let actual = clone_url_digest("https://example.invalid/owner/repository.git");

    assert_eq!(actual.as_str(), EXPECTED_CLONE_URL_DIGEST);
}

#[test]
fn workspace_manifest_digest_is_deterministic() {
    let fixture = manifest();
    let first = workspace_manifest_digest(&fixture)
        .unwrap_or_else(|error| panic!("manifest digests: {error}"));
    let second = workspace_manifest_digest(&fixture)
        .unwrap_or_else(|error| panic!("manifest digests again: {error}"));

    assert_eq!(first, second);
}

#[test]
fn workspace_manifest_digest_preimage_is_pinned() {
    let actual = workspace_manifest_digest(&manifest())
        .unwrap_or_else(|error| panic!("manifest digests: {error}"));

    assert_eq!(actual.as_str(), EXPECTED_MANIFEST_DIGEST);
}

#[test]
fn unborn_workspace_manifest_digest_preimage_is_pinned() {
    let mut fixture = manifest();
    fixture.recovery = Some(Recovery::UnbornBranch {
        name: "main".to_owned(),
    });
    let actual = workspace_manifest_digest(&fixture)
        .unwrap_or_else(|error| panic!("unborn manifest digests: {error}"));

    assert_eq!(actual.as_str(), EXPECTED_UNBORN_MANIFEST_DIGEST);
}

#[test]
fn digest_kinds_are_domain_separated() {
    let advertisement = advertisement_digest(&advertisement())
        .unwrap_or_else(|error| panic!("advertisement digests: {error}"));
    let clone_url = clone_url_digest(EXPECTED_ADVERTISEMENT_DIGEST);

    assert_ne!(advertisement, clone_url);
}

#[test]
fn advertisement_rejects_duplicate_repository_key_with_different_profile() {
    let mut invalid = advertisement();
    invalid.credential_profiles.push(profile("secondary"));
    invalid.repositories.push(RepositoryEntry {
        key: repository("primary"),
        credential_profile: Some(profile("secondary")),
    });

    assert!(invalid.validate().is_err());
}

#[test]
fn failure_detail_rejects_empty_message() {
    assert!(
        FailureDetail::try_new(detail_name("runner.failure"), String::new(), json!({})).is_err()
    );
}

#[test]
fn ready_frame_rejects_manifest_correlation_disagreement() {
    let mut ready_manifest = manifest();
    ready_manifest.session = uuid(ARBITRARY_UUID_A);
    let digest = workspace_manifest_digest(&ready_manifest)
        .unwrap_or_else(|error| panic!("changed manifest digests: {error}"));
    let invalid = Message::WorkspaceReady(WorkspaceReady {
        correlation: provision_correlation(),
        ready: ReadyManifest::try_new(
            ready_manifest,
            digest,
            working_directory("/runner/sessions/cross-wired/repo"),
        )
        .unwrap_or_else(|error| panic!("cross-wired ready manifest is valid: {error}")),
    });

    assert!(Frame::try_new(invalid).is_err());
}

#[test]
fn ready_frame_rejects_a_relative_execution_directory() {
    let ready_manifest = manifest();
    let manifest_digest = workspace_manifest_digest(&ready_manifest)
        .unwrap_or_else(|error| panic!("ready manifest digests: {error}"));

    assert!(
        ReadyManifest::try_new(
            ready_manifest,
            manifest_digest,
            working_directory("sessions/ready/repo"),
        )
        .is_err()
    );
}

#[test]
fn leak_page_rejects_short_nonfinal_page() {
    let report_digest = digest(EXPECTED_ADVERTISEMENT_DIGEST);
    let invalid = leak_page_digest(LeakPageDigestInput {
        registration_revision: positive(7),
        report_digest: &report_digest,
        page: positive(1),
        prior_page_digest: None,
        final_page: false,
        facts: &[],
    });

    assert!(invalid.is_err());
}

#[test]
fn leak_report_rejects_kind_first_instead_of_locator_first_order() {
    let facts = vec![
        LeakFact {
            kind: LeakFactKind::UnknownManifest,
            locator: "z".to_owned(),
            entry_digest: digest(EXPECTED_ADVERTISEMENT_DIGEST),
            session: None,
            placement_revision: None,
        },
        LeakFact {
            kind: LeakFactKind::RetiredPresent,
            locator: "a".to_owned(),
            entry_digest: digest(EXPECTED_CLONE_URL_DIGEST),
            session: None,
            placement_revision: None,
        },
    ];

    assert!(leak_report_digest(&facts).is_err());
}

#[test]
fn reconnect_directives_reject_missing_inventory_member() {
    let inventory = ReconnectInventory {
        lease: Some(LeasePhase {
            correlation: lease_correlation(),
            phase: LeasePhaseKind::WaitingDispatch,
        }),
        ..ReconnectInventory::default()
    };

    assert!(
        ReconnectDirectives::default()
            .validate_against(&inventory)
            .is_err()
    );
}

#[test]
fn reconnect_directives_reject_mismatched_correlation() {
    let inventory = ReconnectInventory {
        lease: Some(LeasePhase {
            correlation: lease_correlation(),
            phase: LeasePhaseKind::WaitingDispatch,
        }),
        ..ReconnectInventory::default()
    };
    let mut mismatch = lease_correlation();
    mismatch.lease_generation = positive(3);
    let directives = ReconnectDirectives {
        lease: Some(Directive {
            correlation: mismatch,
            action: DirectiveAction::Resend,
        }),
        ..ReconnectDirectives::default()
    };

    assert!(directives.validate_against(&inventory).is_err());
}

#[test]
fn resumed_frame_rejects_invalid_complete_provision_correlation() {
    let mut correlation = provision_correlation();
    correlation.repository = None;
    let invalid = Message::Resumed(Box::new(Resumed {
        registration_revision: positive(7),
        connection_epoch: positive(8),
        directives: ReconnectDirectives {
            workspace_operation: Some(Directive {
                correlation: OperationCorrelation::Provision(correlation),
                action: DirectiveAction::Resend,
            }),
            ..ReconnectDirectives::default()
        },
    }));

    assert!(Frame::try_new(invalid).is_err());
}

#[test]
fn resumed_frame_rejects_lease_offer_workspace_correlation() {
    let invalid = Message::Resumed(Box::new(Resumed {
        registration_revision: positive(7),
        connection_epoch: positive(8),
        directives: ReconnectDirectives {
            workspace_operation: Some(Directive {
                correlation: OperationCorrelation::LeaseOffer(lease_correlation()),
                action: DirectiveAction::Resend,
            }),
            ..ReconnectDirectives::default()
        },
    }));

    assert!(Frame::try_new(invalid).is_err());
}

#[test]
fn failure_detail_rejects_payload_string_over_message_bound() {
    let payload = json!({"message": "x".repeat(MAX_FAILURE_MESSAGE_BYTES + 1)});

    assert!(
        FailureDetail::try_new(
            detail_name("runner.failure"),
            "synthetic failure".to_owned(),
            payload,
        )
        .is_err()
    );
}

#[test]
fn failure_detail_rejects_signed_payload_number() {
    assert!(
        FailureDetail::try_new(
            detail_name("runner.failure"),
            "synthetic failure".to_owned(),
            json!({"number": -1}),
        )
        .is_err()
    );
}

#[test]
fn rejected_frame_rejects_free_form_offending_kind() {
    let invalid = Message::Rejected(Rejected {
        offending_kind: "not a frame kind".to_owned(),
        available_correlation: AvailableCorrelation::None,
        code: RejectionCode::MalformedFrame,
    });

    assert!(Frame::try_new(invalid).is_err());
}

#[test]
fn rejected_frame_rejects_oversized_offending_kind() {
    let invalid = Message::Rejected(Rejected {
        offending_kind: "x".repeat(65),
        available_correlation: AvailableCorrelation::None,
        code: RejectionCode::MalformedFrame,
    });

    assert!(Frame::try_new(invalid).is_err());
}

#[test]
fn rejected_frame_rejects_invalid_complete_provision_correlation() {
    let mut correlation = provision_correlation();
    correlation.repository = None;
    let invalid = Message::Rejected(Rejected {
        offending_kind: "workspace_provision".to_owned(),
        available_correlation: AvailableCorrelation::Provision(correlation),
        code: RejectionCode::CorrelationMismatch,
    });

    assert!(Frame::try_new(invalid).is_err());
}

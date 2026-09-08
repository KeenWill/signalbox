use super::support::*;
use crate::*;

#[test]
fn credential_exclusion_responses_reject_zero_generations() -> Result<(), Box<dyn std::error::Error>>
{
    let target = CredentialExclusionTarget::ProfileQuarantine {
        profile: "work".into(),
        record_generation: CanonicalU64::new(0),
    };
    for message in [
        ServerMessage::CredentialExclusion {
            target: target.clone(),
        },
        ServerMessage::CredentialExclusionCleared {
            target: target.clone(),
            outcome: CredentialExclusionClearOutcome::Cleared,
        },
        ServerMessage::CredentialExclusionEnd {
            exclusion_count: CanonicalU64::new(1),
            next_after: Some(target),
        },
    ] {
        assert_eq!(
            ServerFrame::try_new(request(1)?, message.clone()),
            Err(FrameValidationError::CredentialExclusionShape),
            "{message:?}"
        );
    }
    Ok(())
}

#[test]
fn credential_exclusion_empty_page_cannot_continue() -> Result<(), Box<dyn std::error::Error>> {
    let message = ServerMessage::CredentialExclusionEnd {
        exclusion_count: CanonicalU64::new(0),
        next_after: Some(CredentialExclusionTarget::ProfileQuarantine {
            profile: "work".into(),
            record_generation: CanonicalU64::new(1),
        }),
    };
    assert_eq!(
        ServerFrame::try_new(request(1)?, message),
        Err(FrameValidationError::CredentialExclusionShape)
    );
    Ok(())
}

#[test]
fn credential_exclusion_targets_preserve_generations_and_reject_chain_clearing()
-> Result<(), Box<dyn std::error::Error>> {
    let target = CredentialExclusionTarget::ProfileQuarantine {
        profile: "work".into(),
        record_generation: CanonicalU64::new(u64::MAX),
    };
    let frame = ClientFrame::try_new_for_version(
        ProtocolVersion::One,
        request(1)?,
        ClientRequest::ListCredentialExclusions {
            page_size: 1,
            after: Some(target.clone()),
        },
    )?;
    assert_eq!(decode_client_line(&encode_client_line(&frame)?)?, frame);
    assert!(
        serde_json::from_str::<CredentialExclusionTarget>(
            r#"{"type":"chain_exclusion","profile":"work","record_generation":"1"}"#
        )
        .is_err()
    );
    assert!(serde_json::from_str::<CredentialExclusionTarget>(r#"{"type":"profile_quarantine","profile":"work","record_generation":"1","origin":"oauth_refresh"}"#).is_err());
    for page_size in [0, 101] {
        assert!(
            ClientFrame::try_new_for_version(
                ProtocolVersion::One,
                request(1)?,
                ClientRequest::ListCredentialExclusions {
                    page_size,
                    after: Some(target.clone())
                }
            )
            .is_err()
        );
    }
    Ok(())
}

#[test]
fn credential_exclusion_order_uses_scope_fields_before_profile_and_numeric_generation()
-> Result<(), Box<dyn std::error::Error>> {
    let first_policy = CanonicalUuid::from_uuid(uuid::Uuid::from_u128(1));
    let next_policy = CanonicalUuid::from_uuid(uuid::Uuid::from_u128(2));
    let targets = [
        CredentialExclusionTarget::ProfileQuarantine {
            profile: "z".into(),
            record_generation: CanonicalU64::new(10),
        },
        CredentialExclusionTarget::MembershipExclusion {
            pool_policy_id: first_policy,
            profile: "z".into(),
            record_generation: CanonicalU64::new(9),
        },
        CredentialExclusionTarget::MembershipExclusion {
            pool_policy_id: first_policy,
            profile: "z".into(),
            record_generation: CanonicalU64::new(10),
        },
        CredentialExclusionTarget::MembershipExclusion {
            pool_policy_id: next_policy,
            profile: "a".into(),
            record_generation: CanonicalU64::new(1),
        },
        CredentialExclusionTarget::SessionDisplacement {
            session_id: first_policy,
            pool_policy_id: first_policy,
            profile: "a".into(),
            record_generation: CanonicalU64::new(1),
        },
    ];
    let mut actual = targets.iter().rev().cloned().collect::<Vec<_>>();
    actual.sort();
    assert_eq!(actual, targets);
    Ok(())
}

//! Immutable pool reads and storage-to-wire evidence projection.
use super::*;
use signalbox_persistence::credential_pool_exhaustion as store;
use signalbox_process_protocol::{
    CredentialPoolExclusion as WireExclusion, CredentialPoolMemberEvidence,
};

pub(super) fn wire_members(
    members: &[store::CredentialPoolMemberEvidence],
) -> Vec<CredentialPoolMemberEvidence> {
    members
        .iter()
        .map(|member| CredentialPoolMemberEvidence {
            profile: member.profile.clone(),
            reset_at_unix_ms: member.reset_at_unix_ms,
            exclusion: match member.exclusion {
                store::CredentialPoolExclusion::ProfileQuarantine { record_generation } => {
                    WireExclusion::ProfileQuarantine {
                        record_generation: CanonicalU64::new(record_generation.unwrap_or(0)),
                    }
                }
                store::CredentialPoolExclusion::MembershipExclusion { record_generation } => {
                    WireExclusion::MembershipExclusion {
                        record_generation: CanonicalU64::new(record_generation.unwrap_or(0)),
                    }
                }
                store::CredentialPoolExclusion::SessionDisplacement { record_generation } => {
                    WireExclusion::SessionDisplacement {
                        record_generation: CanonicalU64::new(record_generation.unwrap_or(0)),
                    }
                }
                store::CredentialPoolExclusion::ChainExclusion {
                    predecessor_model_call_id,
                } => WireExclusion::ChainExclusion {
                    predecessor_model_call_id: wire_uuid(predecessor_model_call_id),
                },
                store::CredentialPoolExclusion::TransientExclusion {
                    observation_model_call_id,
                } => WireExclusion::TransientExclusion {
                    observation_model_call_id: wire_uuid(observation_model_call_id),
                },
                store::CredentialPoolExclusion::HeadroomReserve {
                    observed_headroom_percent,
                    reserve_percent,
                } => WireExclusion::HeadroomReserve {
                    observed_headroom_percent,
                    reserve_percent,
                },
            },
        })
        .collect()
}

pub(super) async fn handle_read_policy<Writer: AsyncWrite + Unpin>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    session_id: CanonicalUuid,
    turn_id: CanonicalUuid,
    pool_policy_id: CanonicalUuid,
    services: &ConnectionServices,
) -> Result<(), ProcessConnectionError> {
    let outcome = async {
        let mut connection = services.pool.acquire().await?;
        store::read_policy(
            &mut connection,
            session_id.into_uuid(),
            turn_id.into_uuid(),
            pool_policy_id.into_uuid(),
        )
        .await
    }
    .await;
    match outcome {
        Ok(Some(policy_members)) => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::CredentialPoolPolicy {
                    pool_policy_id,
                    policy_members,
                },
            )
            .await
        }
        Ok(None) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::rejected(RejectionDetail::UnknownPoolPolicy {
                    session_id,
                    turn_id,
                    pool_policy_id,
                }),
            )
            .await
        }
        Err(error) => {
            write_process_read_error(
                writer,
                version,
                request_id,
                Some(session_id),
                match error {
                    store::CredentialPoolEvidenceError::Database(error) => {
                        ProcessReadError::from(error)
                    }
                    store::CredentialPoolEvidenceError::Corruption => {
                        signalbox_persistence::process_read::ProcessReadCorruption::Inconsistent(
                            "credential pool policy",
                        )
                        .into()
                    }
                },
            )
            .await
        }
    }
}

//! Verifies frozen exhaustion inventories before exposing them.
use super::*;
use signalbox_process_protocol::{CredentialPoolMemberEvidence, valid_credential_pool_evidence};

pub(crate) async fn validate(
    client: &mut ProcessClient,
    session_id: CanonicalUuid,
    turn_id: CanonicalUuid,
    pool_policy_id: CanonicalUuid,
    policy_members: &[String],
    members: &[CredentialPoolMemberEvidence],
) -> Result<(), ClientError> {
    if !valid_credential_pool_evidence(policy_members, members) {
        return Err(ClientError::Protocol(
            "inconsistent pool exhaustion evidence",
        ));
    }
    let mut read = client
        .request(ClientRequest::ReadCredentialPoolPolicy {
            session_id,
            turn_id,
            pool_policy_id,
        })
        .await?;
    match read.message().await? {
        ServerMessage::CredentialPoolPolicy {
            pool_policy_id: actual,
            policy_members: actual_members,
        } if actual == pool_policy_id && actual_members == policy_members => Ok(()),
        _ => Err(ClientError::Protocol(
            "exhaustion does not match its immutable pool policy",
        )),
    }
}

pub(crate) async fn validate_event(
    client: &mut ProcessClient,
    session_id: CanonicalUuid,
    event: &SessionEvent,
) -> Result<(), ClientError> {
    if let SessionEvent::TurnCredentialPoolExhausted {
        turn_id,
        terminal_attempt_id,
        terminal_frontier_id,
        failure_entry_id,
        pool_policy_id,
        policy_members,
        members,
    } = event
    {
        let mut snapshot = crate::transcript_command(client, session_id).await?;
        let state = snapshot.turn_state(*turn_id)?;
        if !matches!(state, Some(TurnState::FailedCredentialPoolExhausted {
            terminal_attempt_id: actual_attempt, terminal_frontier_id: actual_frontier,
            failure_entry_id: actual_failure, pool_policy_id: actual_policy,
            policy_members: actual_inventory, members: actual_members,
        }) if actual_attempt == *terminal_attempt_id && actual_frontier == *terminal_frontier_id
            && actual_failure == *failure_entry_id && actual_policy == *pool_policy_id
            && actual_inventory == *policy_members && actual_members == *members)
        {
            return Err(ClientError::Protocol(
                "pool exhaustion event disagrees with its snapshot",
            ));
        }
    }
    Ok(())
}

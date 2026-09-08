use super::*;
use signalbox_process_protocol::CredentialExclusionTarget;

pub(crate) async fn list(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    page_size: u32,
    after: Option<CredentialExclusionTarget>,
) -> Result<(), ClientError> {
    let mut connection = client
        .request(ClientRequest::ListCredentialExclusions {
            page_size,
            after: after.clone(),
        })
        .await?;
    match connection.message().await? {
        ServerMessage::CredentialExclusionStart {} => {}
        ServerMessage::Error {
            code,
            message,
            detail,
        } => return Err(ClientError::remote(code, message, detail)),
        _ => {
            return Err(ClientError::Protocol(
                "credential exclusions omitted its page start",
            ));
        }
    }
    let mut targets = Vec::new();
    let next_after = loop {
        match connection.message().await? {
            ServerMessage::CredentialExclusion { target } => {
                if targets.len() >= page_size as usize
                    || targets
                        .last()
                        .or(after.as_ref())
                        .is_some_and(|previous| previous >= &target)
                {
                    return Err(ClientError::Protocol(
                        "credential exclusions returned an invalid page order",
                    ));
                }
                targets.push(target);
            }
            ServerMessage::CredentialExclusionEnd {
                exclusion_count,
                next_after,
            } => {
                if exclusion_count.value() != targets.len() as u64
                    || next_after
                        .as_ref()
                        .is_some_and(|cursor| targets.last() != Some(cursor))
                {
                    return Err(ClientError::Protocol(
                        "credential exclusions returned an inconsistent page end",
                    ));
                }
                break next_after;
            }
            ServerMessage::Error {
                code,
                message,
                detail,
            } => return Err(ClientError::remote(code, message, detail)),
            _ => {
                return Err(ClientError::Protocol(
                    "credential exclusions returned an unexpected response",
                ));
            }
        }
    };
    for target in targets {
        output.credential_exclusion(&target)?;
    }
    if let Some(target) = next_after {
        output.credential_exclusion_cursor(&target)?;
    }
    Ok(())
}
pub(crate) async fn clear(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    target: CredentialExclusionTarget,
    command_id: Option<CommandId>,
) -> Result<(), ClientError> {
    let (command_id, generated) = command_identity(command_id)?;
    if generated {
        output.recovery_value("command_id", &command_id.into_uuid().to_string())?;
    }
    let mut connection = client
        .mutation_request(ClientRequest::ClearCredentialExclusion {
            command_id,
            target: target.clone(),
        })
        .await?;
    match connection.message().await.map_err(ClientError::mutation)? {
        ServerMessage::CredentialExclusionCleared {
            target: recorded,
            outcome,
        } if recorded == target => {
            output.credential_exclusion_cleared(&target, outcome)?;
            Ok(())
        }
        ServerMessage::Error {
            code,
            message,
            detail,
        } => Err(ClientError::remote(code, message, detail).mutation()),
        _ => Err(
            ClientError::Protocol("credential clear returned an unexpected response").mutation(),
        ),
    }
}

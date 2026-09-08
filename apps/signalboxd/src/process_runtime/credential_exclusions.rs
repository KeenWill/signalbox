use super::*;
use signalbox_persistence::credential_exclusions::{
    self as store, CredentialExclusionTarget as StoredTarget,
};
use signalbox_process_protocol::{
    CredentialExclusionClearOutcome, CredentialExclusionTarget as WireTarget,
};

pub(super) async fn handle_list_credential_exclusions<Writer: AsyncWrite + Unpin>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    page_size: u32,
    after: Option<WireTarget>,
    services: &ConnectionServices,
) -> Result<(), ProcessConnectionError> {
    let after = after.map(stored_target);
    let page = match store::list(&services.pool, page_size, after.as_ref()).await {
        Ok(page) => page,
        Err(error) => {
            return write_error(writer, version, request_id, exclusion_error(error)).await;
        }
    };
    write_message(
        writer,
        version,
        request_id,
        ServerMessage::CredentialExclusionStart {},
    )
    .await?;
    let exclusion_count = CanonicalU64::new(page.exclusions.len() as u64);
    for target in page.exclusions {
        write_message(
            writer,
            version,
            request_id,
            ServerMessage::CredentialExclusion {
                target: wire_target(target),
            },
        )
        .await?;
    }
    write_message(
        writer,
        version,
        request_id,
        ServerMessage::CredentialExclusionEnd {
            exclusion_count,
            next_after: page.next_after.map(wire_target),
        },
    )
    .await
}

pub(super) async fn handle_clear_credential_exclusion<Writer: AsyncWrite + Unpin>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    command_id: signalbox_process_protocol::CommandId,
    target: WireTarget,
    services: &ConnectionServices,
) -> Result<(), ProcessConnectionError> {
    use store::{
        ClearCredentialExclusionOutcome as Outcome, ClearCredentialExclusionResult as Result,
    };
    let command = store::ClearCredentialExclusion {
        command_id: DurableCommandId::from_uuid(command_id.into_uuid()),
        target: stored_target(target.clone()),
    };
    let result = match store::clear(&services.pool, command).await {
        Ok(Result::Recorded(Outcome::Cleared)) => Ok(CredentialExclusionClearOutcome::Cleared),
        Ok(Result::Recorded(Outcome::AlreadyCleared)) => {
            Ok(CredentialExclusionClearOutcome::AlreadyCleared)
        }
        Ok(Result::Recorded(Outcome::StaleGeneration)) => {
            Err(ProtocolError::rejected(RejectionDetail::StaleGeneration {}))
        }
        Ok(Result::Recorded(Outcome::UnknownCredentialExclusion)) => Err(ProtocolError::rejected(
            RejectionDetail::UnknownCredentialExclusion {},
        )),
        Ok(Result::ConflictingReuse) => {
            Err(ProtocolError::without_detail(ErrorCode::ConflictingReuse))
        }
        Err(error) => Err(exclusion_error(error)),
    };
    match result {
        Ok(outcome) => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::CredentialExclusionCleared { target, outcome },
            )
            .await
        }
        Err(error) => write_error(writer, version, request_id, error).await,
    }
}

fn exclusion_error(error: store::CredentialExclusionError) -> ProtocolError {
    match error {
        store::CredentialExclusionError::Database(_) => {
            unavailable_protocol_error(InternalDiagnostic::CredentialExclusionDatabase)
        }
        store::CredentialExclusionError::CommitAmbiguous(_) => {
            ProtocolError::mutation_commit_ambiguous()
        }
        store::CredentialExclusionError::Corruption => {
            internal_protocol_error(None, InternalDiagnostic::CredentialExclusionCorruption)
        }
        store::CredentialExclusionError::InvalidRequest => {
            ProtocolError::without_detail(ErrorCode::InvalidRequest)
        }
    }
}

fn stored_target(target: WireTarget) -> StoredTarget {
    match target {
        WireTarget::ProfileQuarantine {
            profile,
            record_generation,
        } => StoredTarget::ProfileQuarantine {
            profile,
            record_generation: record_generation.value(),
        },
        WireTarget::MembershipExclusion {
            pool_policy_id,
            profile,
            record_generation,
        } => StoredTarget::MembershipExclusion {
            pool_policy_id: pool_policy_id.into_uuid(),
            profile,
            record_generation: record_generation.value(),
        },
        WireTarget::SessionDisplacement {
            session_id,
            pool_policy_id,
            profile,
            record_generation,
        } => StoredTarget::SessionDisplacement {
            session_id: session_id.into_uuid(),
            pool_policy_id: pool_policy_id.into_uuid(),
            profile,
            record_generation: record_generation.value(),
        },
    }
}
fn wire_target(target: StoredTarget) -> WireTarget {
    match target {
        StoredTarget::ProfileQuarantine {
            profile,
            record_generation,
        } => WireTarget::ProfileQuarantine {
            profile,
            record_generation: CanonicalU64::new(record_generation),
        },
        StoredTarget::MembershipExclusion {
            pool_policy_id,
            profile,
            record_generation,
        } => WireTarget::MembershipExclusion {
            pool_policy_id: wire_uuid(pool_policy_id),
            profile,
            record_generation: CanonicalU64::new(record_generation),
        },
        StoredTarget::SessionDisplacement {
            session_id,
            pool_policy_id,
            profile,
            record_generation,
        } => WireTarget::SessionDisplacement {
            session_id: wire_uuid(session_id),
            pool_policy_id: wire_uuid(pool_policy_id),
            profile,
            record_generation: CanonicalU64::new(record_generation),
        },
    }
}

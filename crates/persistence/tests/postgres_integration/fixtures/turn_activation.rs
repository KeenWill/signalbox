//! Queued-turn activation and admission.

use crate::*;

pub(crate) async fn run_mixed_occupied_acceptances(
    repository: SubmitInputRepository,
) -> Result<(Vec<u64>, u64, u64), Box<dyn Error>> {
    let mut tasks = Vec::new();
    for offset in 0..6_u128 {
        let repository = repository.clone();
        tasks.push(tokio::spawn(async move {
            let delivery = if offset % 2 == 0 {
                DeliveryRequest::AfterCurrentTurn {
                    expected_active_turn: TurnId::from_uuid(Uuid::from_u128(0xa51)),
                    configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
                }
            } else {
                DeliveryRequest::NextSafePoint {
                    expected_active_turn: TurnId::from_uuid(Uuid::from_u128(0xa51)),
                }
            };
            repository
                .handle(
                    input_with_delivery(
                        0x453 + offset,
                        0x851,
                        &format!("mixed occupied {offset}"),
                        delivery,
                    ),
                    AcceptedInputId::from_uuid(Uuid::from_u128(0x952 + offset)),
                    (offset % 2 == 0).then(|| TurnId::from_uuid(Uuid::from_u128(0xa52 + offset))),
                )
                .await
        }));
    }

    let mut positions = Vec::new();
    let mut turn_origins = 0_u64;
    let mut pending_steering = 0_u64;
    for task in tasks {
        let SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(applied)) =
            task.await??
        else {
            panic!("each mixed occupied-slot submission must apply");
        };
        positions.push(applied.acceptance_position().as_u64());
        match applied {
            SubmitInputAppliedResult::TurnOrigin(_) => turn_origins += 1,
            SubmitInputAppliedResult::PendingSteering(_) => pending_steering += 1,
        }
    }
    positions.sort_unstable();
    Ok((positions, turn_origins, pending_steering))
}

pub(crate) async fn insert_cross_wired_occupied_rejection(
    pool: &PgPool,
    command_id: Uuid,
    source_command_id: Uuid,
    expected_active_turn_id: Uuid,
) -> Result<(), sqlx::Error> {
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at, issuer_kind)
         SELECT $1, command_kind, storage_version, transaction_timestamp(), 'operator'
           FROM durable_command
          WHERE command_id = $2",
    )
    .bind(command_id)
    .bind(source_command_id)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO submit_input_command
            (command_id, command_kind, storage_version, session_id,
             actor_kind, actor_turn_id, actor_tool_request_id,
             delivery_kind, descendant_scope,
             expected_active_turn_id, expected_defaults_version,
             model_override_kind, replacement_model_kind,
             replacement_direct_model_selection_id, replacement_model_alias_id,
             result_kind, rejection_kind, result_session_id,
             result_accepted_input_id, result_turn_id,
             result_actual_active_turn_id, result_expected_active_turn_id,
             result_expected_defaults_version, result_current_defaults_version,
             result_unknown_alias_id, result_selected_defaults_version,
             result_last_position)
         SELECT
             $1, command_kind, storage_version, session_id,
             actor_kind, actor_turn_id, actor_tool_request_id,
             delivery_kind, descendant_scope,
             $3, expected_defaults_version,
             model_override_kind, replacement_model_kind,
             replacement_direct_model_selection_id, replacement_model_alias_id,
             result_kind, rejection_kind, result_session_id,
             result_accepted_input_id, result_turn_id,
             result_actual_active_turn_id, result_expected_active_turn_id,
             result_expected_defaults_version, result_current_defaults_version,
             result_unknown_alias_id, result_selected_defaults_version,
             result_last_position
           FROM submit_input_command
          WHERE command_id = $2",
    )
    .bind(command_id)
    .bind(source_command_id)
    .bind(expected_active_turn_id)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO submit_input_command_content_part
            (command_id, position, part_kind, text_value, blob_digest,
             attachment_kind, declared_media_type, display_filename)
         SELECT $1, position, part_kind, text_value, blob_digest,
                attachment_kind, declared_media_type, display_filename
           FROM submit_input_command_content_part
          WHERE command_id = $2",
    )
    .bind(command_id)
    .bind(source_command_id)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await
}

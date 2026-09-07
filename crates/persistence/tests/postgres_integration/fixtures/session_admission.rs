//! Session admission and input projections.

use crate::*;

pub(crate) struct TwoValueTail<'a, Value> {
    pub(crate) penultimate: &'a Value,
    pub(crate) last: &'a Value,
}

#[track_caller]
pub(crate) fn last_two<Value>(values: &[Value]) -> TwoValueTail<'_, Value> {
    let [.., penultimate, last] = values else {
        panic!("fixture must carry a two-value tail");
    };
    TwoValueTail { penultimate, last }
}

#[track_caller]
pub(crate) fn application_user_message(
    message: &ModelConversationMessage,
) -> (AcceptedInputId, &str) {
    match message {
        ModelConversationMessage::User {
            accepted_input,
            content,
            ..
        } => (
            *accepted_input,
            content
                .single_text()
                .expect("the fixture has exactly one text part")
                .as_str(),
        ),
        _ => panic!("fixture message must be an application user-role message"),
    }
}

#[track_caller]
pub(crate) fn application_model_identity(
    message: &ModelConversationMessage,
) -> (u64, DirectModelSelection) {
    match message {
        ModelConversationMessage::ModelIdentityChanged {
            defaults_version,
            selected,
            ..
        } => (defaults_version.as_u64(), *selected),
        _ => panic!("fixture message must be an application model-identity boundary"),
    }
}

#[track_caller]
pub(crate) fn submit_input_database_error(error: SubmitInputRepositoryError) -> sqlx::Error {
    match error {
        SubmitInputRepositoryError::Database(error) => error,
        error => panic!("fixture expected a submit-input database error, got {error:?}"),
    }
}

#[track_caller]
pub(crate) fn process_user_entry(
    entry: &ProcessTranscriptEntry,
) -> (AcceptedInputId, TurnId, &str) {
    match entry {
        ProcessTranscriptEntry::User {
            accepted_input,
            turn,
            content,
            ..
        } => (
            *accepted_input,
            *turn,
            content
                .single_text()
                .expect("fixture process content is one text part")
                .as_str(),
        ),
        _ => panic!("fixture entry must be a process user entry"),
    }
}

#[track_caller]
pub(crate) fn process_model_identity(
    entry: &ProcessTranscriptEntry,
) -> (TurnId, u64, DirectModelSelection) {
    match entry {
        ProcessTranscriptEntry::ModelIdentityChanged {
            turn,
            defaults_version,
            selected,
            ..
        } => (*turn, *defaults_version, *selected),
        _ => panic!("fixture entry must be a process model-identity boundary"),
    }
}

#[derive(Debug, PartialEq, sqlx::FromRow)]
pub(crate) struct ModelCallPinFacts {
    pub(crate) direct_model_selection_id: Uuid,
    pub(crate) resolved_provider_model_identity_id: Uuid,
    pub(crate) credential_reference: String,
}

pub(crate) async fn record_stale_active_input(
    repository: &SubmitInputRepository,
    command_value: u128,
    delivery: DeliveryRequest,
    accepted_input: u128,
    turn: Option<u128>,
) -> Result<(SubmitInput, SubmitInputHandlingOutcome), SubmitInputRepositoryError> {
    let command = input_with_delivery(command_value, 0x841, "stale active", delivery);
    let outcome = repository
        .handle(
            command.clone(),
            AcceptedInputId::from_uuid(Uuid::from_u128(accepted_input)),
            turn.map(|value| TurnId::from_uuid(Uuid::from_u128(value))),
        )
        .await?;
    Ok((command, outcome))
}

pub(crate) async fn active_origin_collision(
    repository: &SubmitInputRepository,
    pool: &PgPool,
    command_id: DurableCommandId,
    session: SessionId,
    active_origin_input: AcceptedInputId,
    delivery: DeliveryRequest,
    turn: Option<u128>,
) -> Result<(SubmitInputRepositoryError, i64), Box<dyn Error>> {
    let command = input_with_delivery(
        command_id.into_uuid().as_u128(),
        session.into_uuid().as_u128(),
        "colliding active origin",
        delivery,
    );
    let error = repository
        .handle(
            command,
            active_origin_input,
            turn.map(|value| TurnId::from_uuid(Uuid::from_u128(value))),
        )
        .await
        .expect_err("new acceptance cannot reuse the active origin identity");
    let claimed = sqlx::query_scalar("SELECT count(*) FROM durable_command WHERE command_id = $1")
        .bind(command_id.into_uuid())
        .fetch_one(pool)
        .await?;
    Ok((error, claimed))
}

pub(crate) fn replacement_request(
    command: u128,
    session: u128,
    expected: u64,
    selection: ModelSelectionRequest,
) -> ReplaceSessionDefaultsRequest {
    ReplaceSessionDefaultsRequest::try_new(
        DurableCommandId::from_uuid(Uuid::from_u128(command)),
        SessionId::from_uuid(Uuid::from_u128(session)),
        SessionConfigurationDefaultsVersion::try_from_u64(expected)
            .expect("test versions are positive"),
        SessionConfigurationDefaults::new(selection),
        PromptMemberStatement::Stated,
    )
    .expect("ordinary test command identities are admitted")
}

/// Builds one submit input whose content optionally carries a blob attachment.
///
/// The attachment part travels with the parent command so persistence writes
/// both in the creating transaction, which content-part immutability requires.
pub(crate) fn start_input_with_attachment(
    command: u128,
    session: u128,
    content: &str,
    expected: u64,
    model: ModelSelectionOverride,
    attachment: Option<BlobDigest>,
) -> SubmitInput {
    let mut parts =
        vec![UserContentPart::try_text(content.to_owned()).expect("test content is admitted")];
    if let Some(digest) = attachment {
        parts.push(UserContentPart::Attachment {
            digest,
            kind: AttachmentKind::File,
            media_type: DeclaredMediaType::try_new(String::from("application/octet-stream"))
                .expect("the fixture media type is admitted"),
            display_filename: Some(
                AttachmentDisplayFilename::try_new(String::from("fixture.bin"))
                    .expect("the fixture basename is admitted"),
            ),
        });
    }
    SubmitInput::new(
        DurableCommandId::from_uuid(Uuid::from_u128(command)),
        SessionId::from_uuid(Uuid::from_u128(session)),
        UserContent::try_parts(parts).expect("the fixture parts form admitted content"),
        DeliveryRequest::StartWhenNoActiveTurn {
            configuration: input_choices(expected, model),
        },
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn insert_malformed_submit_rejection(
    pool: &PgPool,
    command_id: Uuid,
    source_command_id: Uuid,
    rejection_kind: &str,
    result_expected_active_turn: Option<Uuid>,
    result_expected_defaults: Option<Decimal>,
    result_current_defaults: Option<Decimal>,
    result_unknown_alias: Option<Uuid>,
    result_selected_defaults: Option<Decimal>,
    result_last_position: Option<Decimal>,
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
             result_expected_active_turn_id, result_expected_defaults_version,
             result_current_defaults_version, result_unknown_alias_id,
             result_selected_defaults_version, result_last_position)
         SELECT
             $1, command_kind, storage_version, session_id,
             actor_kind, actor_turn_id, actor_tool_request_id,
             delivery_kind, descendant_scope,
             expected_active_turn_id, expected_defaults_version,
             model_override_kind, replacement_model_kind,
             replacement_direct_model_selection_id, replacement_model_alias_id,
             'rejected', $3, result_session_id,
             NULL, NULL, $4, $5, $6, $7, $8, $9
           FROM submit_input_command
          WHERE command_id = $2",
    )
    .bind(command_id)
    .bind(source_command_id)
    .bind(rejection_kind)
    .bind(result_expected_active_turn)
    .bind(result_expected_defaults)
    .bind(result_current_defaults)
    .bind(result_unknown_alias)
    .bind(result_selected_defaults)
    .bind(result_last_position)
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

#[track_caller]
pub(crate) fn applied_session(outcome: CreateSessionOutcome) -> SessionId {
    let CreateSessionOutcome::Applied(applied) = outcome else {
        panic!("fixture session creation must apply")
    };
    applied.session()
}

pub(crate) fn create_session_corruption(
    error: CreateSessionRepositoryError,
) -> CreateSessionCorruption {
    let CreateSessionRepositoryError::Corruption(corruption) = error else {
        panic!("the ordinary creation reader failure is durable corruption")
    };
    corruption
}

pub(crate) fn session_corruption(error: SessionRepositoryError) -> SessionCorruption {
    let SessionRepositoryError::Corruption(corruption) = error else {
        panic!("the current-session reader failure is durable corruption")
    };
    corruption
}

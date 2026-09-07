//! Outbox dispatch and delivery.

use crate::*;

#[derive(Clone, Copy, Debug)]
pub(crate) struct RecordedSettingsReplacement {
    pub(crate) session: SessionId,
    pub(crate) command: DurableCommandId,
}

pub(crate) async fn record_settings_replacement_fixture(
    pool: &PgPool,
    seed: u128,
) -> Result<RecordedSettingsReplacement, Box<dyn Error>> {
    let session = SessionId::from_uuid(Uuid::from_u128(seed + 1));
    let initial_selection = DirectModelSelection::from_uuid(Uuid::from_u128(seed + 2));
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared_with_low_reasoning(
            seed + 3,
            seed + 1,
            initial_selection,
        ))
        .await?;
    let installed_selection = DirectModelSelection::from_uuid(Uuid::from_u128(seed + 4));
    let caller_settings = ModelSettingsOverlay::new(
        SettingOverlay::ProviderDefault,
        FastModeOverlay::Inherit,
        SettingOverlay::Inherit,
    );
    let installed_settings = ModelCapabilities::new(
        BTreeSet::new(),
        FastModeSupport::Unsupported,
        BTreeSet::new(),
    )
    .validate_precedence(
        installed_selection,
        ModelSettingsPrecedence::new(
            ModelSettingsOverlay::inherit_all(),
            caller_settings,
            ModelSettingsOverlay::inherit_all(),
            ModelSettingsOverlay::inherit_all(),
        ),
    )
    .expect("the provider-default fixture is valid for the replacement");
    let installed_defaults = SessionConfigurationDefaults::complete_with_model_settings(
        ModelSelectionRequest::Direct(installed_selection),
        signalbox_domain::DangerousToolAutoApproval::Disabled,
        None,
        installed_settings,
    )
    .expect("the replacement settings belong to the direct selection");
    let command = DurableCommandId::from_uuid(Uuid::from_u128(seed + 5));
    let replacement = ReplaceSessionDefaults::with_model_settings(
        command,
        session,
        SessionConfigurationDefaultsVersion::try_from_u64(1)
            .expect("the fixture version is positive"),
        installed_defaults,
        caller_settings,
    );
    ReplaceSessionDefaultsRepository::new(pool.clone())
        .handle(replacement)
        .await?;
    Ok(RecordedSettingsReplacement { session, command })
}

pub(crate) async fn record_settings_replacement(
    pool: &PgPool,
    seed: u128,
) -> Result<SessionId, Box<dyn Error>> {
    Ok(record_settings_replacement_fixture(pool, seed)
        .await?
        .session)
}

pub(crate) async fn append_session_created_test_event(
    connection: &mut PgConnection,
    session: Uuid,
) -> Result<Decimal, sqlx::Error> {
    let sequence = sqlx::query_scalar(
        "INSERT INTO outbox_event
            (event_kind, storage_version, session_id)
         VALUES ('session_created', 2, $1)
         RETURNING event_sequence",
    )
    .bind(session)
    .fetch_one(&mut *connection)
    .await?;

    sqlx::query(
        "INSERT INTO session_created_outbox_event
            (event_sequence, event_kind, storage_version, session_id,
             creation_cause, owned)
         VALUES ($1, 'session_created', 2, $2, 'interactive', false)",
    )
    .bind(sequence)
    .bind(session)
    .execute(&mut *connection)
    .await?;

    Ok(sequence)
}

pub(crate) async fn assert_outbox_truncate_rejected(
    pool: &PgPool,
    statement: &'static str,
) -> Result<(), Box<dyn Error>> {
    let error = sqlx::query(statement)
        .execute(pool)
        .await
        .expect_err("outbox storage is not removable through truncate");
    assert_eq!(
        error
            .as_database_error()
            .and_then(|database| database.code())
            .as_deref(),
        Some("23514")
    );
    Ok(())
}

/// Derives the direct model selection installed by the outbox session fixture.
pub(crate) fn outbox_session_fixture_model_selection(session_seed: u128) -> DirectModelSelection {
    DirectModelSelection::from_uuid(Uuid::from_u128(session_seed ^ 0x2000))
}

pub(crate) async fn corrupt_ended_attempt_disposition(
    pool: &PgPool,
    attempt: TurnAttemptId,
    disposition: &'static str,
) -> Result<(), sqlx::Error> {
    sqlx::query("ALTER TABLE turn_attempt DISABLE TRIGGER USER")
        .execute(pool)
        .await?;
    sqlx::query(
        "UPDATE turn_attempt
            SET end_disposition = $1
          WHERE turn_attempt_id = $2",
    )
    .bind(disposition)
    .bind(attempt.into_uuid())
    .execute(pool)
    .await?;
    sqlx::query("ALTER TABLE turn_attempt ENABLE TRIGGER USER")
        .execute(pool)
        .await?;
    Ok(())
}

pub(crate) fn user_content(value: &str) -> UserContent {
    UserContent::try_text(value.to_owned()).expect("test content is admitted")
}

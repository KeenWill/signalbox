//! Shared storage of settings evidence for every accepted turn origin.

use signalbox_domain::{
    AcceptedInputId, OriginConfiguration, SessionId, TurnId, TurnModelSettingsResolved,
};
use sqlx::PgConnection;

use crate::{
    mapping::{
        accepted_input_id_to_uuid, defaults_version_to_numeric, model_change_adjustments_to_json,
        model_settings_overlay_to_json, model_settings_to_json, session_id_to_uuid,
        turn_id_to_uuid,
    },
    outbox::{self, OutboxEvent},
};

pub(crate) fn event_from_origin(
    accepted_input: AcceptedInputId,
    turn: TurnId,
    configuration: &OriginConfiguration,
) -> Option<TurnModelSettingsResolved> {
    TurnModelSettingsResolved::try_new(
        accepted_input,
        turn,
        configuration.session_defaults_version(),
        *configuration.effective().model(),
        configuration.requested().per_call_model_settings(),
        configuration.effective().model_settings(),
        configuration.model_settings_adjusted_from(),
        configuration.model_settings_adjustments().to_vec(),
    )
}

pub(crate) async fn persist(
    connection: &mut PgConnection,
    session: SessionId,
    event: &TurnModelSettingsResolved,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO turn_model_settings_resolved
            (accepted_input_id, turn_id, session_id, defaults_version,
             selected_direct_model_id, per_call_model_settings,
             resolved_model_settings, adjusted_from_selection_id, adjustments)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(accepted_input_id_to_uuid(event.accepted_input()))
    .bind(turn_id_to_uuid(event.turn()))
    .bind(session_id_to_uuid(session))
    .bind(defaults_version_to_numeric(event.defaults_version()))
    .bind(event.selection().selected_direct().into_uuid())
    .bind(model_settings_overlay_to_json(event.per_call_override()))
    .bind(model_settings_to_json(event.settings()))
    .bind(
        event
            .adjusted_from_selection()
            .map(signalbox_domain::DirectModelSelection::into_uuid),
    )
    .bind(model_change_adjustments_to_json(event.adjustments()))
    .execute(&mut *connection)
    .await?;
    outbox::append(
        connection,
        OutboxEvent::TurnModelSettingsResolved {
            session,
            accepted_input: event.accepted_input(),
        },
    )
    .await
}

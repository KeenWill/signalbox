//! Shared storage of settings evidence for every accepted turn origin.

use signalbox_domain::{
    AcceptedInputId, EffectiveModelSettings, FastMode, FastModeOverlay, ModelChangeAdjustment,
    ModelSettingSource, ModelSettingsOverlay, ModelSettingsPrecedence, OriginConfiguration,
    ResolvedModelSettings, SessionId, SettingOverlay, TurnId, TurnModelSettingsResolved,
    ValidatedModelSettings,
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

pub(crate) fn matches_defaults(
    event: &TurnModelSettingsResolved,
    defaults: ValidatedModelSettings,
) -> bool {
    let expected = defaults
        .precedence()
        .with_per_call(event.per_call_override());
    let settled = event.settings().precedence();
    let restored = restore_adjusted_precedence(event.settings(), event.adjustments());
    let adjustment_origin_matches = event.adjustments().is_empty()
        || event.adjusted_from_selection() == defaults.validated_for();
    restored == Some(expected)
        && settled.per_call() == event.per_call_override()
        && adjustment_origin_matches
}

fn restore_adjusted_precedence(
    settings: ValidatedModelSettings,
    adjustments: &[ModelChangeAdjustment],
) -> Option<ModelSettingsPrecedence> {
    let settled = settings.resolved();
    let mut prior = settled.effective();
    for adjustment in adjustments {
        prior = match adjustment {
            ModelChangeAdjustment::ReasoningLevelClamped { from, to }
                if settled.reasoning_source() != Some(ModelSettingSource::PerCall)
                    && settled.effective().reasoning_level() == Some(*to) =>
            {
                EffectiveModelSettings::new(Some(*from), prior.fast_mode(), prior.service_tier())
            }
            ModelChangeAdjustment::ReasoningLevelCleared { from }
                if settled.reasoning_source() != Some(ModelSettingSource::PerCall)
                    && settled.effective().reasoning_level().is_none() =>
            {
                EffectiveModelSettings::new(Some(*from), prior.fast_mode(), prior.service_tier())
            }
            ModelChangeAdjustment::FastModeDisabled
                if settled.fast_mode_source() != Some(ModelSettingSource::PerCall)
                    && settled.effective().fast_mode() == FastMode::Disabled =>
            {
                EffectiveModelSettings::new(
                    prior.reasoning_level(),
                    FastMode::Enabled,
                    prior.service_tier(),
                )
            }
            ModelChangeAdjustment::ServiceTierCleared { from }
                if settled.service_tier_source() != Some(ModelSettingSource::PerCall)
                    && settled.effective().service_tier().is_none() =>
            {
                EffectiveModelSettings::new(prior.reasoning_level(), prior.fast_mode(), Some(*from))
            }
            ModelChangeAdjustment::ReasoningLevelClamped { .. }
            | ModelChangeAdjustment::ReasoningLevelCleared { .. }
            | ModelChangeAdjustment::FastModeDisabled
            | ModelChangeAdjustment::ServiceTierCleared { .. } => return None,
        };
    }
    restore_precedence_effective(settings.precedence(), settled, prior)
}

fn restore_precedence_effective(
    precedence: ModelSettingsPrecedence,
    settled: ResolvedModelSettings,
    prior: EffectiveModelSettings,
) -> Option<ModelSettingsPrecedence> {
    let mut layers = [
        precedence.per_call(),
        precedence.session(),
        precedence.profile(),
        precedence.global_default(),
    ];
    let source_index = |source| match source {
        ModelSettingSource::PerCall => 0,
        ModelSettingSource::Session => 1,
        ModelSettingSource::Profile => 2,
        ModelSettingSource::GlobalDefault => 3,
    };
    if settled.effective().reasoning_level() != prior.reasoning_level() {
        let index = source_index(settled.reasoning_source()?);
        layers[index] = ModelSettingsOverlay::new(
            prior
                .reasoning_level()
                .map_or(SettingOverlay::ProviderDefault, SettingOverlay::Value),
            layers[index].fast_mode(),
            layers[index].service_tier(),
        );
    }
    if settled.effective().fast_mode() != prior.fast_mode() {
        let index = source_index(settled.fast_mode_source()?);
        layers[index] = ModelSettingsOverlay::new(
            layers[index].reasoning_level(),
            FastModeOverlay::Value(prior.fast_mode()),
            layers[index].service_tier(),
        );
    }
    if settled.effective().service_tier() != prior.service_tier() {
        let index = source_index(settled.service_tier_source()?);
        layers[index] = ModelSettingsOverlay::new(
            layers[index].reasoning_level(),
            layers[index].fast_mode(),
            prior
                .service_tier()
                .map_or(SettingOverlay::ProviderDefault, SettingOverlay::Value),
        );
    }
    Some(ModelSettingsPrecedence::new(
        layers[0], layers[1], layers[2], layers[3],
    ))
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use signalbox_domain::{
        AcceptedInputId, DirectModelSelection, FastModeSupport, FrozenModelSelection,
        ModelCapabilities, ModelSettingsOverlay, ModelSettingsPrecedence, ReasoningLevel,
        SettingOverlay, TurnId, TurnModelSettingsResolved,
    };
    use sqlx::types::Uuid;

    use super::matches_defaults;

    #[test]
    fn adjusted_turn_settings_authenticate_against_their_unadjusted_defaults() {
        let prior_selection = DirectModelSelection::from_uuid(Uuid::from_u128(1));
        let selected = DirectModelSelection::from_uuid(Uuid::from_u128(2));
        let per_call = ModelSettingsOverlay::inherit_all();
        let defaults_precedence = ModelSettingsPrecedence::new(
            per_call,
            ModelSettingsOverlay::new(
                SettingOverlay::Value(ReasoningLevel::High),
                signalbox_domain::FastModeOverlay::Inherit,
                SettingOverlay::Inherit,
            ),
            ModelSettingsOverlay::inherit_all(),
            ModelSettingsOverlay::inherit_all(),
        );
        let defaults = ModelCapabilities::new(
            BTreeSet::from([ReasoningLevel::High]),
            FastModeSupport::Unsupported,
            BTreeSet::new(),
        )
        .validate_precedence(prior_selection, defaults_precedence)
        .expect("the prior capability admits high reasoning");
        let adjusted = ModelCapabilities::new(
            BTreeSet::new(),
            FastModeSupport::Unsupported,
            BTreeSet::new(),
        )
        .validate_model_change(selected, defaults_precedence, per_call)
        .expect("the model change clears inherited reasoning");
        let (settings, adjustments) = adjusted.into_parts();
        let event = TurnModelSettingsResolved::try_new(
            AcceptedInputId::from_uuid(Uuid::from_u128(3)),
            TurnId::from_uuid(Uuid::from_u128(4)),
            signalbox_domain::SessionConfigurationDefaultsVersion::try_from_u64(1)
                .expect("the fixture version is positive"),
            FrozenModelSelection::Direct(selected),
            per_call,
            settings,
            Some(prior_selection),
            adjustments.into_vec(),
        )
        .expect("the adjusted settings evidence is internally consistent");

        assert!(matches_defaults(&event, defaults));
        assert!(!matches_defaults(
            &event,
            signalbox_domain::ValidatedModelSettings::provider_defaults()
        ));
        assert_ne!(event.selection().selected_direct(), prior_selection);
    }
}

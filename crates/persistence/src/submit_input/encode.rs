use super::load::configured_defaults_version;
use super::{APPLIED, REJECTED, descendant_scope_to_str};
use crate::mapping::{
    AttachmentRejectionStorageKind, accepted_input_id_to_uuid, attachment_rejection_kind_to_str,
    defaults_version_to_numeric, durable_command_id_to_uuid, input_position_to_numeric,
    model_settings_overlay_to_json, turn_id_to_uuid,
};
use rust_decimal::Decimal;
use serde_json::Value;
use signalbox_domain::{
    Actor, AttachmentDisplayFilename, AttachmentKind, DeliveryRequest, FrozenModelSelection,
    ModelSelectionOverride, ModelSelectionRequest, PerInputConfigurationChoices, SessionId,
    SubmitInputAppliedResult, SubmitInputRejectedResult, SubmitInputResult, UserContentPart,
};
use sqlx::types::Uuid;

pub(super) struct EncodedContentPart<'a> {
    pub(super) kind: &'static str,
    pub(super) text: Option<&'a str>,
    pub(super) digest: Option<&'a [u8]>,
    pub(super) attachment_kind: Option<&'static str>,
    pub(super) media_type: Option<&'a str>,
    pub(super) filename: Option<&'a str>,
}

pub(super) fn encode_content_part(part: &UserContentPart) -> EncodedContentPart<'_> {
    match part {
        UserContentPart::Text { value } => EncodedContentPart {
            kind: "text",
            text: Some(value.as_str()),
            digest: None,
            attachment_kind: None,
            media_type: None,
            filename: None,
        },
        UserContentPart::Attachment {
            digest,
            kind,
            media_type,
            display_filename,
        } => EncodedContentPart {
            kind: "attachment",
            text: None,
            digest: Some(digest.as_bytes()),
            attachment_kind: Some(match kind {
                AttachmentKind::Image => "image",
                AttachmentKind::Document => "document",
                AttachmentKind::File => "file",
            }),
            media_type: Some(media_type.as_str()),
            filename: display_filename
                .as_ref()
                .map(AttachmentDisplayFilename::as_str),
        },
    }
}

pub(super) struct EncodedActor {
    pub(super) kind: &'static str,
    pub(super) turn: Option<Uuid>,
    pub(super) tool_request: Option<Uuid>,
    pub(super) program_run: Option<Uuid>,
}

pub(super) fn encode_actor(actor: Actor) -> EncodedActor {
    match actor {
        Actor::Program { run } => EncodedActor {
            kind: "program",
            turn: None,
            tool_request: None,
            program_run: Some(run.run().into_uuid()),
        },
        Actor::User => EncodedActor {
            program_run: None,
            kind: "user",
            turn: None,
            tool_request: None,
        },
        Actor::Core => EncodedActor {
            program_run: None,
            kind: "core",
            turn: None,
            tool_request: None,
        },
        Actor::Model { turn } => EncodedActor {
            program_run: None,
            kind: "model",
            turn: Some(turn.into_uuid()),
            tool_request: None,
        },
        Actor::Recovery => EncodedActor {
            program_run: None,
            kind: "recovery",
            turn: None,
            tool_request: None,
        },
        Actor::Tool { request } => EncodedActor {
            program_run: None,
            kind: "tool",
            turn: None,
            tool_request: Some(request.into_uuid()),
        },
    }
}

#[derive(Clone, Copy)]
pub(super) struct EncodedSelection {
    pub(super) kind: Option<&'static str>,
    pub(super) direct: Option<Uuid>,
    pub(super) alias: Option<Uuid>,
}

impl EncodedSelection {
    const fn absent() -> Self {
        Self {
            kind: None,
            direct: None,
            alias: None,
        }
    }
}

pub(super) fn encode_selection(selection: ModelSelectionRequest) -> EncodedSelection {
    match selection {
        ModelSelectionRequest::Direct(selection) => EncodedSelection {
            kind: Some("direct"),
            direct: Some(selection.into_uuid()),
            alias: None,
        },
        ModelSelectionRequest::Alias(alias) => EncodedSelection {
            kind: Some("alias"),
            direct: None,
            alias: Some(alias.into_uuid()),
        },
    }
}

pub(super) struct EncodedFrozenModel {
    pub(super) kind: &'static str,
    pub(super) direct: Option<Uuid>,
    pub(super) alias: Option<Uuid>,
    pub(super) alias_selected: Option<Uuid>,
}

pub(super) fn encode_frozen_model(model: &FrozenModelSelection) -> EncodedFrozenModel {
    match model {
        FrozenModelSelection::Direct(selection) => EncodedFrozenModel {
            kind: "direct",
            direct: Some(selection.into_uuid()),
            alias: None,
            alias_selected: None,
        },
        FrozenModelSelection::FrozenAlias { alias, definition } => EncodedFrozenModel {
            kind: "frozen_alias",
            direct: None,
            alias: Some(alias.into_uuid()),
            alias_selected: Some(definition.selected().into_uuid()),
        },
    }
}

pub(super) struct EncodedDelivery {
    pub(super) kind: &'static str,
    pub(super) descendant_scope: Option<&'static str>,
    pub(super) expected_active_turn: Option<Uuid>,
    pub(super) expected_defaults_version: Option<Decimal>,
    pub(super) model_override_kind: Option<&'static str>,
    pub(super) replacement: EncodedSelection,
    pub(super) model_settings: Value,
}

pub(super) fn encode_delivery(delivery: DeliveryRequest) -> EncodedDelivery {
    match delivery {
        DeliveryRequest::StartWhenNoActiveTurn { configuration } => {
            encode_configured_delivery("start_when_no_active_turn", None, configuration)
        }
        DeliveryRequest::Interrupt {
            expected_active_turn,
            descendant_scope,
            configuration,
        } => {
            let mut encoded = encode_configured_delivery(
                "interrupt",
                Some(expected_active_turn.into_uuid()),
                configuration,
            );
            encoded.descendant_scope = Some(descendant_scope_to_str(descendant_scope));
            encoded
        }
        DeliveryRequest::NextSafePoint {
            expected_active_turn,
        } => EncodedDelivery {
            kind: "next_safe_point",
            descendant_scope: None,
            expected_active_turn: Some(expected_active_turn.into_uuid()),
            expected_defaults_version: None,
            model_override_kind: None,
            replacement: EncodedSelection::absent(),
            model_settings: model_settings_overlay_to_json(
                signalbox_domain::ModelSettingsOverlay::inherit_all(),
            ),
        },
        DeliveryRequest::AfterCurrentTurn {
            expected_active_turn,
            configuration,
        } => encode_configured_delivery(
            "after_current_turn",
            Some(expected_active_turn.into_uuid()),
            configuration,
        ),
    }
}

fn encode_configured_delivery(
    kind: &'static str,
    expected_active_turn: Option<Uuid>,
    configuration: PerInputConfigurationChoices,
) -> EncodedDelivery {
    let (model_override_kind, replacement) = match configuration.model() {
        ModelSelectionOverride::UseSessionDefault => {
            ("use_session_default", EncodedSelection::absent())
        }
        ModelSelectionOverride::ReplaceWith(selection) => {
            ("replace_with", encode_selection(selection))
        }
    };
    EncodedDelivery {
        kind,
        descendant_scope: None,
        expected_active_turn,
        expected_defaults_version: Some(defaults_version_to_numeric(
            configuration.expected_session_defaults_version(),
        )),
        model_override_kind: Some(model_override_kind),
        replacement,
        model_settings: model_settings_overlay_to_json(configuration.model_settings()),
    }
}

pub(super) struct EncodedResult {
    pub(super) kind: &'static str,
    pub(super) rejection_kind: Option<&'static str>,
    pub(super) session: SessionId,
    pub(super) accepted_input: Option<Uuid>,
    pub(super) turn: Option<Uuid>,
    pub(super) actual_active_turn: Option<Uuid>,
    pub(super) expected_active_turn: Option<Uuid>,
    pub(super) expected_defaults_version: Option<Decimal>,
    pub(super) current_defaults_version: Option<Decimal>,
    pub(super) unknown_alias: Option<Uuid>,
    pub(super) selected_defaults_version: Option<Decimal>,
    pub(super) last_position: Option<Decimal>,
    pub(super) existing_interrupt_command: Option<Uuid>,
    pub(super) attachment_digest: Option<Vec<u8>>,
    pub(super) attachment_maximum_bytes: Option<Decimal>,
}

pub(super) fn encode_result(
    result: &SubmitInputResult,
    delivery: DeliveryRequest,
    command_session: SessionId,
) -> EncodedResult {
    match result {
        SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(result)) => EncodedResult {
            kind: APPLIED,
            rejection_kind: None,
            session: result.session(),
            accepted_input: Some(accepted_input_id_to_uuid(result.accepted_input())),
            turn: Some(turn_id_to_uuid(result.turn())),
            actual_active_turn: None,
            expected_active_turn: None,
            expected_defaults_version: None,
            current_defaults_version: None,
            unknown_alias: None,
            selected_defaults_version: None,
            last_position: None,
            existing_interrupt_command: None,
            attachment_digest: None,
            attachment_maximum_bytes: None,
        },
        SubmitInputResult::Applied(SubmitInputAppliedResult::PendingSteering(result)) => {
            EncodedResult {
                kind: APPLIED,
                rejection_kind: None,
                session: result.session(),
                accepted_input: Some(accepted_input_id_to_uuid(result.accepted_input())),
                turn: None,
                actual_active_turn: Some(turn_id_to_uuid(result.binding().source_turn())),
                expected_active_turn: None,
                expected_defaults_version: None,
                current_defaults_version: None,
                unknown_alias: None,
                selected_defaults_version: None,
                last_position: None,
                existing_interrupt_command: None,
                attachment_digest: None,
                attachment_maximum_bytes: None,
            }
        }
        SubmitInputResult::Rejected(SubmitInputRejectedResult::AttachmentBlobNotFound {
            digest,
        }) => EncodedResult {
            kind: REJECTED,
            rejection_kind: Some(attachment_rejection_kind_to_str(
                AttachmentRejectionStorageKind::BlobNotFound,
            )),
            session: command_session,
            accepted_input: None,
            turn: None,
            actual_active_turn: None,
            expected_active_turn: None,
            expected_defaults_version: None,
            current_defaults_version: None,
            unknown_alias: None,
            selected_defaults_version: None,
            last_position: None,
            existing_interrupt_command: None,
            attachment_digest: Some(digest.as_bytes().to_vec()),
            attachment_maximum_bytes: None,
        },
        SubmitInputResult::Rejected(SubmitInputRejectedResult::AttachmentByteBudgetExceeded {
            maximum_bytes,
        }) => EncodedResult {
            kind: REJECTED,
            rejection_kind: Some(attachment_rejection_kind_to_str(
                AttachmentRejectionStorageKind::ByteBudgetExceeded,
            )),
            session: command_session,
            accepted_input: None,
            turn: None,
            actual_active_turn: None,
            expected_active_turn: None,
            expected_defaults_version: None,
            current_defaults_version: None,
            unknown_alias: None,
            selected_defaults_version: None,
            last_position: None,
            existing_interrupt_command: None,
            attachment_digest: None,
            attachment_maximum_bytes: Some(Decimal::from(*maximum_bytes)),
        },
        SubmitInputResult::Rejected(SubmitInputRejectedResult::ActiveTurnPresent {
            session,
            active_turn,
        }) => EncodedResult {
            kind: REJECTED,
            rejection_kind: Some("active_turn_present"),
            session: *session,
            accepted_input: None,
            turn: None,
            actual_active_turn: Some(turn_id_to_uuid(*active_turn)),
            expected_active_turn: None,
            expected_defaults_version: None,
            current_defaults_version: None,
            unknown_alias: None,
            selected_defaults_version: None,
            last_position: None,
            existing_interrupt_command: None,
            attachment_digest: None,
            attachment_maximum_bytes: None,
        },
        SubmitInputResult::Rejected(SubmitInputRejectedResult::ActiveTurnMismatch {
            session,
            expected_active_turn,
            actual_active_turn,
        }) => EncodedResult {
            kind: REJECTED,
            rejection_kind: Some("active_turn_mismatch"),
            session: *session,
            accepted_input: None,
            turn: None,
            actual_active_turn: Some(turn_id_to_uuid(*actual_active_turn)),
            expected_active_turn: Some(turn_id_to_uuid(*expected_active_turn)),
            expected_defaults_version: None,
            current_defaults_version: None,
            unknown_alias: None,
            selected_defaults_version: None,
            last_position: None,
            existing_interrupt_command: None,
            attachment_digest: None,
            attachment_maximum_bytes: None,
        },
        SubmitInputResult::Rejected(SubmitInputRejectedResult::SessionNotFound { session }) => {
            EncodedResult {
                kind: REJECTED,
                rejection_kind: Some("session_not_found"),
                session: *session,
                accepted_input: None,
                turn: None,
                actual_active_turn: None,
                expected_active_turn: None,
                expected_defaults_version: None,
                current_defaults_version: None,
                unknown_alias: None,
                selected_defaults_version: None,
                last_position: None,
                existing_interrupt_command: None,
                attachment_digest: None,
                attachment_maximum_bytes: None,
            }
        }
        SubmitInputResult::Rejected(SubmitInputRejectedResult::NoActiveTurn {
            session,
            expected_active_turn,
        }) => EncodedResult {
            kind: REJECTED,
            rejection_kind: Some("no_active_turn"),
            session: *session,
            accepted_input: None,
            turn: None,
            actual_active_turn: None,
            expected_active_turn: Some(turn_id_to_uuid(*expected_active_turn)),
            expected_defaults_version: None,
            current_defaults_version: None,
            unknown_alias: None,
            selected_defaults_version: None,
            last_position: None,
            existing_interrupt_command: None,
            attachment_digest: None,
            attachment_maximum_bytes: None,
        },
        SubmitInputResult::Rejected(
            SubmitInputRejectedResult::SessionDefaultsVersionMismatch {
                session,
                expected,
                current,
            },
        ) => EncodedResult {
            kind: REJECTED,
            rejection_kind: Some("session_defaults_version_mismatch"),
            session: *session,
            accepted_input: None,
            turn: None,
            actual_active_turn: None,
            expected_active_turn: None,
            expected_defaults_version: Some(defaults_version_to_numeric(*expected)),
            current_defaults_version: Some(defaults_version_to_numeric(*current)),
            unknown_alias: None,
            selected_defaults_version: None,
            last_position: None,
            existing_interrupt_command: None,
            attachment_digest: None,
            attachment_maximum_bytes: None,
        },
        SubmitInputResult::Rejected(SubmitInputRejectedResult::UnknownModelAlias {
            session,
            alias,
        }) => EncodedResult {
            kind: REJECTED,
            rejection_kind: Some("unknown_model_alias"),
            session: *session,
            accepted_input: None,
            turn: None,
            actual_active_turn: None,
            expected_active_turn: None,
            expected_defaults_version: None,
            current_defaults_version: None,
            unknown_alias: Some(alias.into_uuid()),
            selected_defaults_version: configured_defaults_version(delivery)
                .map(defaults_version_to_numeric),
            last_position: None,
            existing_interrupt_command: None,
            attachment_digest: None,
            attachment_maximum_bytes: None,
        },
        SubmitInputResult::Rejected(SubmitInputRejectedResult::AcceptancePositionExhausted {
            session,
            last,
        }) => EncodedResult {
            kind: REJECTED,
            rejection_kind: Some("acceptance_position_exhausted"),
            session: *session,
            accepted_input: None,
            turn: None,
            actual_active_turn: None,
            expected_active_turn: None,
            expected_defaults_version: None,
            current_defaults_version: None,
            unknown_alias: None,
            selected_defaults_version: None,
            last_position: Some(input_position_to_numeric(*last)),
            existing_interrupt_command: None,
            attachment_digest: None,
            attachment_maximum_bytes: None,
        },
        SubmitInputResult::Rejected(
            SubmitInputRejectedResult::SafePointUnavailableWhileStopping {
                session,
                active_turn,
                existing_command,
            },
        ) => EncodedResult {
            kind: REJECTED,
            rejection_kind: Some("safe_point_unavailable_while_stopping"),
            session: *session,
            accepted_input: None,
            turn: None,
            actual_active_turn: Some(turn_id_to_uuid(*active_turn)),
            expected_active_turn: None,
            expected_defaults_version: None,
            current_defaults_version: None,
            unknown_alias: None,
            selected_defaults_version: None,
            last_position: None,
            existing_interrupt_command: Some(durable_command_id_to_uuid(*existing_command)),
            attachment_digest: None,
            attachment_maximum_bytes: None,
        },
        SubmitInputResult::Rejected(SubmitInputRejectedResult::InterruptAlreadyApplied {
            session,
            active_turn,
            existing_command,
        }) => EncodedResult {
            kind: REJECTED,
            rejection_kind: Some("interrupt_already_applied"),
            session: *session,
            accepted_input: None,
            turn: None,
            actual_active_turn: Some(turn_id_to_uuid(*active_turn)),
            expected_active_turn: None,
            expected_defaults_version: None,
            current_defaults_version: None,
            unknown_alias: None,
            selected_defaults_version: None,
            last_position: None,
            existing_interrupt_command: Some(durable_command_id_to_uuid(*existing_command)),
            attachment_digest: None,
            attachment_maximum_bytes: None,
        },
        SubmitInputResult::Rejected(
            SubmitInputRejectedResult::InterruptUnavailableWhileAwaitingApproval {
                session,
                active_turn,
            },
        ) => EncodedResult {
            kind: REJECTED,
            rejection_kind: Some("interrupt_unavailable_while_awaiting_approval"),
            session: *session,
            accepted_input: None,
            turn: None,
            actual_active_turn: Some(turn_id_to_uuid(*active_turn)),
            expected_active_turn: None,
            expected_defaults_version: None,
            current_defaults_version: None,
            unknown_alias: None,
            selected_defaults_version: None,
            last_position: None,
            existing_interrupt_command: None,
            attachment_digest: None,
            attachment_maximum_bytes: None,
        },
    }
}

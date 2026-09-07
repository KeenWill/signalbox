use super::{ModelCallCorruption, ModelCallRepositoryError, map_scheduling_error, required};
use crate::mapping::{
    durable_command_id_from_uuid, positive_u64_from_numeric, session_id_to_uuid, turn_id_to_uuid,
};
use crate::submit_input::require_recorded_batch;
use signalbox_domain::{
    AcceptedInputId, AttachmentBlobFact, BlobDigest, DirectModelSelection, DurableCommandId,
    EmptyTurnInstructionManifestEvidence, FrozenAliasDefinition, FrozenModelSelection,
    InstructionDigest, ModelAlias, ModelCallDisposition, ModelCallExecution, ModelCallId,
    ModelCallOriginContent, ModelCallReconstitutionInput, ModelCallReconstitutionState,
    PendingSteeringInput, PinnedProviderTargetReconstitutionInput, ProviderModelIdentity,
    ResolvedProviderTarget, SemanticTranscriptEntry, SemanticTranscriptEntryPayload, SessionId,
    TurnId, TurnInstructionManifest, TurnInstructionManifestId, UserContent,
};
use sqlx::postgres::PgRow;
use sqlx::types::Uuid;
use sqlx::{PgConnection, Row};
use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU64;

enum StoredAcceptedInputProvenance {
    Command(DurableCommandId),
    Goal(UserContent),
}

pub(super) async fn load_origin_contents(
    connection: &mut PgConnection,
    entries: &[SemanticTranscriptEntry],
    pending_steering: &[PendingSteeringInput],
    consumed_steering: &[signalbox_domain::ConsumedSteeringInput],
) -> Result<Vec<ModelCallOriginContent>, ModelCallRepositoryError> {
    let pending_by_accepted = pending_steering
        .iter()
        .map(|pending| (pending.accepted_input(), pending))
        .collect::<BTreeMap<_, _>>();
    let consumed_by_accepted = consumed_steering
        .iter()
        .map(|consumed| (consumed.accepted_input(), consumed))
        .collect::<BTreeMap<_, _>>();
    let accepted_inputs = entries
        .iter()
        .filter_map(|entry| match entry.payload() {
            SemanticTranscriptEntryPayload::OriginAcceptedInput { accepted_input }
            | SemanticTranscriptEntryPayload::SteeringAcceptedInput { accepted_input, .. } => {
                Some(*accepted_input)
            }
            SemanticTranscriptEntryPayload::TurnFailed { .. }
            | SemanticTranscriptEntryPayload::DelegatedTask { .. }
            | SemanticTranscriptEntryPayload::DelegationMessage { .. }
            | SemanticTranscriptEntryPayload::DelegationResult { .. }
            | SemanticTranscriptEntryPayload::ModelIdentityChanged { .. }
            | SemanticTranscriptEntryPayload::ContextSummary { .. }
            | SemanticTranscriptEntryPayload::RunnerPlacementChanged { .. }
            | SemanticTranscriptEntryPayload::TurnCancelled { .. }
            | SemanticTranscriptEntryPayload::AssistantText { .. }
            | SemanticTranscriptEntryPayload::ProviderCompaction { .. }
            | SemanticTranscriptEntryPayload::ProviderReasoning { .. }
            | SemanticTranscriptEntryPayload::AssistantToolUse { .. }
            | SemanticTranscriptEntryPayload::ToolExecutionResult { .. }
            | SemanticTranscriptEntryPayload::ToolDenied { .. }
            | SemanticTranscriptEntryPayload::ToolInadmissible { .. }
            | SemanticTranscriptEntryPayload::ToolClosed { .. }
            | SemanticTranscriptEntryPayload::TurnCompleted { .. }
            | SemanticTranscriptEntryPayload::Imported { .. } => None,
        })
        .chain(pending_by_accepted.keys().copied())
        .chain(consumed_by_accepted.keys().copied())
        .collect::<BTreeSet<_>>();
    if accepted_inputs.is_empty() {
        return Ok(Vec::new());
    }
    let accepted_input_uuids = accepted_inputs
        .iter()
        .map(|accepted_input| accepted_input.into_uuid())
        .collect::<Vec<_>>();
    let rows = sqlx::query(
        "SELECT accepted.accepted_input_id, accepted.accepting_command_id,
                accepted_input_content_parts_json(accepted.accepted_input_id)
                    AS content_parts,
                goal.turn_id AS goal_turn_id
           FROM accepted_input AS accepted
           LEFT JOIN goal_turn AS goal
             ON goal.accepted_input_id = accepted.accepted_input_id
          WHERE accepted.accepted_input_id = ANY($1)
          ORDER BY accepted.accepted_input_id",
    )
    .bind(&accepted_input_uuids)
    .fetch_all(&mut *connection)
    .await?;
    if rows.len() != accepted_input_uuids.len() {
        return Err(ModelCallCorruption::Missing("accepted input receipt").into());
    }
    let mut loaded = BTreeSet::new();
    let mut command_by_accepted = BTreeMap::new();
    let mut goal_content_by_accepted = BTreeMap::new();
    let mut steering_content_by_accepted = BTreeMap::new();
    for row in rows {
        let accepted: Uuid = required(&row, "accepted_input_id")?;
        if !accepted_input_uuids.contains(&accepted) || !loaded.insert(accepted) {
            return Err(ModelCallCorruption::Inconsistent("accepted receipt inventory").into());
        }
        let accepted = AcceptedInputId::from_uuid(accepted);
        if pending_by_accepted.contains_key(&accepted)
            || consumed_by_accepted.contains_key(&accepted)
        {
            let content = crate::user_content::decode(required(&row, "content_parts")?)
                .map_err(|_| ModelCallCorruption::Inconsistent("steering content"))?;
            if steering_content_by_accepted
                .insert(accepted, content)
                .is_some()
            {
                return Err(ModelCallCorruption::Inconsistent("accepted receipt inventory").into());
            }
            continue;
        }
        let command: Option<Uuid> = row.try_get("accepting_command_id")?;
        let goal_turn: Option<Uuid> = row.try_get("goal_turn_id")?;
        // An accepting command decides provenance whether or not a generation
        // owns the turn. A commissioned dispatch binds its goal to a turn its
        // input command already accepted, so both rows exist and the text was
        // authored by that command; the `goal_turn` row records which generation
        // the turn runs under, not where its input came from.
        let provenance = match (command, goal_turn) {
            (Some(command), _) => {
                let command = durable_command_id_from_uuid(command)
                    .map_err(|_| ModelCallCorruption::Inconsistent("accepting command identity"))?;
                StoredAcceptedInputProvenance::Command(command)
            }
            (None, Some(_)) => {
                let content = crate::user_content::decode(required(&row, "content_parts")?)
                    .map_err(|_| ModelCallCorruption::Inconsistent("goal input content"))?;
                StoredAcceptedInputProvenance::Goal(content)
            }
            (None, None) => {
                return Err(ModelCallCorruption::Inconsistent("accepted input provenance").into());
            }
        };
        match provenance {
            StoredAcceptedInputProvenance::Command(command) => {
                if command_by_accepted.insert(accepted, command).is_some() {
                    return Err(
                        ModelCallCorruption::Inconsistent("accepted receipt inventory").into(),
                    );
                }
            }
            StoredAcceptedInputProvenance::Goal(content) => {
                if goal_content_by_accepted.insert(accepted, content).is_some() {
                    return Err(
                        ModelCallCorruption::Inconsistent("accepted receipt inventory").into(),
                    );
                }
            }
        }
    }
    let commands = command_by_accepted.values().copied().collect::<Vec<_>>();
    let recorded = require_recorded_batch(connection, &commands)
        .await
        .map_err(map_scheduling_error)?;
    accepted_inputs
        .into_iter()
        .map(|accepted| {
            let content = match steering_content_by_accepted.remove(&accepted) {
                Some(content) if pending_by_accepted.contains_key(&accepted) => {
                    ModelCallOriginContent::from_pending_steering(
                        pending_by_accepted
                            .get(&accepted)
                            .ok_or(ModelCallCorruption::Missing("pending steering correlation"))?,
                        content,
                    )
                }
                Some(content) => ModelCallOriginContent::from_consumed_steering(
                    consumed_by_accepted
                        .get(&accepted)
                        .ok_or(ModelCallCorruption::Missing(
                            "consumed steering correlation",
                        ))?,
                    content,
                ),
                None => match goal_content_by_accepted.remove(&accepted) {
                    Some(content) => ModelCallOriginContent::from_goal_turn(accepted, content),
                    None => {
                        let command = command_by_accepted
                            .get(&accepted)
                            .ok_or(ModelCallCorruption::Missing("accepted command correlation"))?;
                        let submit = recorded
                            .get(command)
                            .ok_or(ModelCallCorruption::Missing("accepted submit command"))?;
                        ModelCallOriginContent::from_recorded_submit(submit)
                            .ok_or(ModelCallCorruption::Inconsistent("accepted input content"))?
                    }
                },
            };
            if content.accepted_input() != accepted {
                return Err(ModelCallCorruption::Inconsistent("accepted content identity").into());
            }
            Ok(content)
        })
        .collect()
}

pub(super) async fn load_attachment_blob_facts(
    connection: &mut PgConnection,
    origin_contents: &[ModelCallOriginContent],
) -> Result<Vec<AttachmentBlobFact>, ModelCallRepositoryError> {
    let digests = origin_contents
        .iter()
        .flat_map(|origin| origin.content().parts())
        .filter_map(|part| match part {
            signalbox_domain::UserContentPart::Attachment { digest, .. } => Some(*digest),
            signalbox_domain::UserContentPart::Text { .. } => None,
        })
        .collect::<BTreeSet<_>>();
    if digests.is_empty() {
        return Ok(Vec::new());
    }
    let encoded = digests
        .iter()
        .map(|digest| digest.as_bytes().to_vec())
        .collect::<Vec<_>>();
    let rows = sqlx::query(
        "SELECT digest, byte_length
           FROM blob
          WHERE digest = ANY($1::bytea[])
          ORDER BY digest",
    )
    .bind(&encoded)
    .fetch_all(&mut *connection)
    .await?;
    if rows.len() != digests.len() {
        return Err(ModelCallCorruption::Missing("attachment blob catalog fact").into());
    }
    rows.into_iter()
        .map(|row| {
            let bytes: Vec<u8> = row.try_get("digest")?;
            let digest = BlobDigest::from_bytes(bytes.try_into().map_err(|_| {
                ModelCallCorruption::Inconsistent("attachment blob catalog digest")
            })?);
            if !digests.contains(&digest) {
                return Err(
                    ModelCallCorruption::Inconsistent("attachment blob catalog inventory").into(),
                );
            }
            let length = positive_u64_from_numeric(row.try_get("byte_length")?).map_err(|_| {
                ModelCallCorruption::Inconsistent("attachment blob catalog byte length")
            })?;
            let length = NonZeroU64::new(length).ok_or(ModelCallCorruption::Inconsistent(
                "attachment blob catalog byte length",
            ))?;
            Ok(AttachmentBlobFact::new(digest, length))
        })
        .collect()
}

pub(super) async fn load_live_turn_calls(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
) -> Result<
    (
        Option<PinnedProviderTargetReconstitutionInput>,
        Vec<ModelCallReconstitutionInput>,
    ),
    ModelCallRepositoryError,
> {
    let lifecycle = sqlx::query(
        "SELECT pinned_provider_model_identity_id, recovery_model_call_id
           FROM turn_lifecycle
          WHERE session_id = $1
            AND turn_id = $2",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .fetch_optional(&mut *connection)
    .await?
    .ok_or(ModelCallCorruption::Missing("live turn lifecycle"))?;
    let pinned_identity: Option<Uuid> = lifecycle.try_get("pinned_provider_model_identity_id")?;
    let pinned_target = pinned_identity.map(|identity| {
        PinnedProviderTargetReconstitutionInput::new(
            turn,
            ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(identity)),
        )
    });
    let recovery_call: Option<Uuid> = lifecycle.try_get("recovery_model_call_id")?;
    let rows = sqlx::query(
        "SELECT call.model_call_id, call.turn_id, call.turn_attempt_id,
                call.selection_kind, call.direct_model_selection_id,
                call.frozen_model_alias_id, call.frozen_alias_selected_direct_id,
                call.resolved_provider_model_identity_id, call.context_frontier_id,
                call.state_kind, call.terminal_disposition_kind,
                manifest.turn_instruction_manifest_id,
                manifest.boundary_kind AS instruction_manifest_boundary_kind,
                manifest.eligibility_hash_algorithm
                    AS instruction_eligibility_hash_algorithm,
                manifest.eligibility_hash AS instruction_eligibility_hash,
                manifest.admitted_set_hash_algorithm
                    AS instruction_admitted_set_hash_algorithm,
                manifest.admitted_set_hash AS instruction_admitted_set_hash,
                manifest.manifest_hash_algorithm
                    AS instruction_manifest_hash_algorithm,
                manifest.manifest_hash AS instruction_manifest_hash,
                discovery.scan_complete AS instruction_discovery_complete
           FROM model_call AS call
      LEFT JOIN turn_instruction_manifest AS manifest
             ON manifest.turn_instruction_manifest_id = call.turn_instruction_manifest_id
            AND manifest.session_id = call.session_id
            AND manifest.turn_id = call.turn_id
      LEFT JOIN instruction_discovery AS discovery
             ON discovery.instruction_discovery_id = manifest.instruction_discovery_id
         WHERE call.session_id = $1
            AND call.turn_id = $2
            AND (
                call.state_kind <> 'terminal'
                OR call.model_call_id = $3
            )
          ORDER BY call.model_call_id",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .bind(recovery_call)
    .fetch_all(&mut *connection)
    .await?;
    Ok((
        pinned_target,
        rows.into_iter()
            .map(|row| decode_model_call(row, session))
            .collect::<Result<_, _>>()?,
    ))
}

pub(super) fn decode_model_call(
    row: PgRow,
    session: SessionId,
) -> Result<ModelCallReconstitutionInput, ModelCallRepositoryError> {
    let turn = TurnId::from_uuid(required(&row, "turn_id")?);
    authenticate_model_call_instruction_manifest(&row, session, turn)?;
    let state_kind: String = required(&row, "state_kind")?;
    let terminal: Option<String> = row.try_get("terminal_disposition_kind")?;
    let state = match (state_kind.as_str(), terminal.as_deref()) {
        ("prepared", None) => ModelCallReconstitutionState::Prepared,
        ("in_flight", None) => ModelCallReconstitutionState::InFlight,
        ("cancellation_requested", None) => ModelCallReconstitutionState::CancellationRequested,
        ("terminal", Some(value)) => {
            ModelCallReconstitutionState::Terminal(decode_disposition(value)?)
        }
        ("prepared" | "in_flight" | "cancellation_requested" | "terminal", _) => {
            return Err(ModelCallCorruption::Inconsistent("model-call state payload").into());
        }
        (value, _) => {
            return Err(ModelCallCorruption::Unsupported {
                field: "model_call.state_kind",
                value: value.to_owned(),
            }
            .into());
        }
    };
    Ok(ModelCallReconstitutionInput::new(
        ModelCallId::from_uuid(required(&row, "model_call_id")?),
        turn,
        signalbox_domain::TurnAttemptId::from_uuid(required(&row, "turn_attempt_id")?),
        decode_selection(
            required(&row, "selection_kind")?,
            row.try_get("direct_model_selection_id")?,
            row.try_get("frozen_model_alias_id")?,
            row.try_get("frozen_alias_selected_direct_id")?,
        )?,
        ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(required(
            &row,
            "resolved_provider_model_identity_id",
        )?)),
        signalbox_domain::ContextFrontierId::from_uuid(required(&row, "context_frontier_id")?),
        state,
    ))
}

pub(crate) fn authenticate_model_call_instruction_manifest(
    row: &PgRow,
    session: SessionId,
    turn: TurnId,
) -> Result<(), ModelCallRepositoryError> {
    let manifest_id =
        TurnInstructionManifestId::from_uuid(required(row, "turn_instruction_manifest_id")?);
    let boundary_kind: String = required(row, "instruction_manifest_boundary_kind")?;
    if boundary_kind != "turn_start" {
        return Err(ModelCallCorruption::Inconsistent("turn instruction manifest boundary").into());
    }
    if !required::<bool>(row, "instruction_discovery_complete")? {
        return Err(ModelCallCorruption::Inconsistent("instruction discovery completeness").into());
    }
    if required::<String>(row, "instruction_eligibility_hash_algorithm")? != "sha256_v1"
        || required::<String>(row, "instruction_admitted_set_hash_algorithm")? != "sha256_v1"
        || required::<String>(row, "instruction_manifest_hash_algorithm")? != "sha256_v1"
    {
        return Err(
            ModelCallCorruption::Inconsistent("turn instruction manifest hash algorithm").into(),
        );
    }
    let eligibility_hash: Vec<u8> = required(row, "instruction_eligibility_hash")?;
    let admitted_set_hash: Vec<u8> = required(row, "instruction_admitted_set_hash")?;
    let manifest_hash: Vec<u8> = required(row, "instruction_manifest_hash")?;
    let eligibility_hash: [u8; 32] = eligibility_hash
        .try_into()
        .map_err(|_| ModelCallCorruption::Inconsistent("instruction eligibility hash"))?;
    let admitted_set_hash: [u8; 32] = admitted_set_hash
        .try_into()
        .map_err(|_| ModelCallCorruption::Inconsistent("instruction admitted-set hash"))?;
    let manifest_hash: [u8; 32] = manifest_hash
        .try_into()
        .map_err(|_| ModelCallCorruption::Inconsistent("instruction manifest hash"))?;
    TurnInstructionManifest::reconstitute_empty_turn_start(
        manifest_id,
        session,
        turn,
        EmptyTurnInstructionManifestEvidence {
            eligibility_hash: InstructionDigest::from_sha256(eligibility_hash),
            admitted_set_hash: InstructionDigest::from_sha256(admitted_set_hash),
            manifest_hash: InstructionDigest::from_sha256(manifest_hash),
        },
    )
    .ok_or(ModelCallCorruption::Inconsistent(
        "turn instruction manifest authentication",
    ))?;
    Ok(())
}

fn decode_selection(
    kind: String,
    direct: Option<Uuid>,
    alias: Option<Uuid>,
    alias_selected: Option<Uuid>,
) -> Result<FrozenModelSelection, ModelCallRepositoryError> {
    match (kind.as_str(), direct, alias, alias_selected) {
        ("direct", Some(direct), None, None) => Ok(FrozenModelSelection::Direct(
            DirectModelSelection::from_uuid(direct),
        )),
        ("frozen_alias", None, Some(alias), Some(selected)) => {
            Ok(FrozenModelSelection::FrozenAlias {
                alias: ModelAlias::from_uuid(alias),
                definition: FrozenAliasDefinition::selecting(DirectModelSelection::from_uuid(
                    selected,
                )),
            })
        }
        ("direct" | "frozen_alias", _, _, _) => {
            Err(ModelCallCorruption::Inconsistent("frozen selection payload").into())
        }
        (value, _, _, _) => Err(ModelCallCorruption::Unsupported {
            field: "model_call.selection_kind",
            value: value.to_owned(),
        }
        .into()),
    }
}

fn decode_disposition(value: &str) -> Result<ModelCallDisposition, ModelCallRepositoryError> {
    match value {
        "completed" => Ok(ModelCallDisposition::Completed),
        "known_failed" => Ok(ModelCallDisposition::KnownFailed),
        "refused" => Ok(ModelCallDisposition::Refused),
        "cancelled" => Ok(ModelCallDisposition::Cancelled),
        "ambiguous" => Ok(ModelCallDisposition::Ambiguous),
        value => Err(ModelCallCorruption::Unsupported {
            field: "model_call.terminal_disposition_kind",
            value: value.to_owned(),
        }
        .into()),
    }
}

pub(super) fn require_exact_call(
    execution: ModelCallExecution,
    call: ModelCallId,
) -> Result<ModelCallExecution, ModelCallRepositoryError> {
    if matches!(execution.current_call(), Some(current) if current.id() == call) {
        Ok(execution)
    } else {
        Err(ModelCallRepositoryError::InvalidTransition(
            "fresh execution does not contain the expected call",
        ))
    }
}

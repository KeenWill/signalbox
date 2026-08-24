//! Production browser adapter for bounded imported-conversation discovery.

use std::{num::NonZeroU32, sync::Arc};

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use signalbox_application::{
    CreateSessionFromImportedFrontierOutcome, CreateSessionFromImportedFrontierRequest,
    CreateSessionFromImportedFrontierService, UuidV7CreateSessionFromImportedFrontierIdGenerator,
};
use signalbox_domain::{
    DirectModelSelection, DurableCommandId, ImportedConversationFormat, ImportedConversationId,
    ImportedSessionRelationship, ImportedSourceAttestation, ImportedSpeaker,
    ImportedTranscriptEntryId, ImportedTranscriptFrontier, ImportedTranscriptPosition, ModelAlias,
    ModelSelectionRequest, SessionConfigurationDefaults,
};
use signalbox_persistence::{
    conversation_import_discovery::{
        ImportedContinuationReference, ImportedConversationDescriptor,
        ImportedConversationDiscoveryError, ImportedConversationDiscoveryRepository,
        ImportedConversationDiscoveryRequestError, ImportedConversationPageRequest,
        ImportedConversationSummary, ImportedEntryContentProjection, ImportedEntryProjection,
        ImportedEntryWindow, ImportedEntryWindowAnchor, ImportedTextProjection,
    },
    create_session_from_imported_frontier::{
        ImportedSessionRepository, ImportedSessionRepositoryError,
    },
};
use signalbox_web_contract::{
    MAX_IMPORT_ENTRY_WINDOW_ITEMS, MAX_IMPORT_LIST_ITEMS, MAX_IMPORT_SOURCE_SESSION_BYTES,
    MAX_IMPORT_TEXT_PREVIEW_BYTES, WebImportContinuationReference, WebImportContinuationRequest,
    WebImportContinuationResponse, WebImportDescriptor, WebImportEntryWindow,
    WebImportEntryWindowRequest, WebImportFormat, WebImportListPage, WebImportListRequest,
    WebImportSizeFacts, WebImportSourceEvidence, WebImportSourceSessionEvidence, WebImportSummary,
    WebImportTextCompleteness, WebImportTextEvidence, WebImportTimelineBounds,
    WebImportWindowAnchor, WebImportedContentKind, WebImportedEntry,
    WebImportedSessionRelationship, WebImportedSpeakerEvidence, WebModelSelection,
};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    HubModelConfiguration,
    web_http::{application_error, decode_bounded_json, transport_error, validate_json_mutation},
};

// Tunable effective ceiling: ordinary catalog views ask for a dense first page below the hard cap.
const DEFAULT_IMPORT_LIST_ITEMS: u32 = 50;
// Tunable effective ceiling: default entry windows retain 25 neighbors on each side.
const DEFAULT_IMPORT_WINDOW_RADIUS: u32 = 25;

#[derive(Clone, Debug)]
struct WebImportState {
    pool: PgPool,
    model_configuration: Arc<HubModelConfiguration>,
}

pub(crate) fn router(pool: PgPool, model_configuration: HubModelConfiguration) -> Router {
    let state = WebImportState {
        pool,
        model_configuration: Arc::new(model_configuration),
    };
    let mutation = Router::new()
        .route("/{conversation}/continuations", post(continue_import))
        .route_layer(middleware::from_fn(validate_json_mutation));
    Router::new()
        .route("/", get(list_imports))
        .route("/{conversation}", get(read_descriptor))
        .route("/{conversation}/entries", get(read_entry_window))
        .merge(mutation)
        .with_state(state)
}

async fn list_imports(
    State(state): State<WebImportState>,
    request: Result<Query<WebImportListRequest>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let Query(request) = match request {
        Ok(request) => request,
        Err(_) => return invalid_request("imports query is malformed"),
    };
    let limit = request.limit.unwrap_or(DEFAULT_IMPORT_LIST_ITEMS);
    let Some(limit) = NonZeroU32::new(limit).filter(|limit| limit.get() <= MAX_IMPORT_LIST_ITEMS)
    else {
        return invalid_request("imports limit is outside the contract bound");
    };
    let after = match optional_uuid(request.after.as_deref()) {
        Ok(after) => after.map(ImportedConversationId::from_uuid),
        Err(message) => return invalid_request(message),
    };
    let Some(source_session_maximum_bytes) = source_session_maximum_bytes() else {
        return invalid_import_contract();
    };
    let query = ImportedConversationPageRequest {
        after,
        format: request.format.map(domain_format),
        source_session_id: request.source_session_id.map(String::into_bytes),
        source_session_maximum_bytes,
        limit,
    };
    match ImportedConversationDiscoveryRepository::new(state.pool)
        .list(query)
        .await
    {
        Ok(page) => Json(WebImportListPage {
            items: page.items.into_iter().map(web_summary).collect(),
            next_cursor: page.next_after.map(|cursor| cursor.into_uuid().to_string()),
        })
        .into_response(),
        Err(error) => discovery_error(error),
    }
}

async fn read_descriptor(
    State(state): State<WebImportState>,
    Path(conversation): Path<String>,
) -> Response {
    let conversation = match imported_conversation_id(&conversation) {
        Ok(conversation) => conversation,
        Err(message) => return invalid_request(message),
    };
    let Some(source_session_maximum_bytes) = source_session_maximum_bytes() else {
        return invalid_import_contract();
    };
    match ImportedConversationDiscoveryRepository::new(state.pool)
        .descriptor(conversation, source_session_maximum_bytes)
        .await
    {
        Ok(Some(descriptor)) => Json(web_descriptor(descriptor)).into_response(),
        Ok(None) => import_not_found(),
        Err(error) => discovery_error(error),
    }
}

async fn read_entry_window(
    State(state): State<WebImportState>,
    Path(conversation): Path<String>,
    request: Result<Query<WebImportEntryWindowRequest>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let conversation = match imported_conversation_id(&conversation) {
        Ok(conversation) => conversation,
        Err(message) => return invalid_request(message),
    };
    let Query(request) = match request {
        Ok(request) => request,
        Err(_) => return invalid_request("imported entry-window query is malformed"),
    };
    let anchor = match web_window_anchor(request.anchor, request.position) {
        Ok(anchor) => anchor,
        Err(message) => return invalid_request(message),
    };
    let before = request.before.unwrap_or(DEFAULT_IMPORT_WINDOW_RADIUS);
    let after = request.after.unwrap_or(DEFAULT_IMPORT_WINDOW_RADIUS);
    let Some(projected_items) = before
        .checked_add(after)
        .and_then(|total| total.checked_add(1))
    else {
        return invalid_request("imported entry-window bound overflows");
    };
    if projected_items > MAX_IMPORT_ENTRY_WINDOW_ITEMS {
        return invalid_request("imported entry window exceeds the contract bound");
    }
    let Some(maximum_items) = NonZeroU32::new(MAX_IMPORT_ENTRY_WINDOW_ITEMS) else {
        return application_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "invalid_import_contract",
            "imported entry-window contract is invalid",
        );
    };
    let Some(maximum_text_bytes) = u32::try_from(MAX_IMPORT_TEXT_PREVIEW_BYTES)
        .ok()
        .and_then(NonZeroU32::new)
    else {
        return invalid_import_contract();
    };
    match ImportedConversationDiscoveryRepository::new(state.pool)
        .entry_window(
            conversation,
            anchor,
            before,
            after,
            maximum_items,
            maximum_text_bytes,
        )
        .await
    {
        Ok(Some(window)) => Json(web_entry_window(window)).into_response(),
        Ok(None) => import_not_found(),
        Err(ImportedConversationDiscoveryError::Request(
            ImportedConversationDiscoveryRequestError::PositionOutOfRange,
        )) => position_out_of_range(),
        Err(error) => discovery_error(error),
    }
}

async fn continue_import(
    State(state): State<WebImportState>,
    Path(conversation): Path<String>,
    request: axum::extract::Request,
) -> Response {
    let conversation = match imported_conversation_id(&conversation) {
        Ok(conversation) => conversation,
        Err(message) => return invalid_request(message),
    };
    let request = match decode_bounded_json::<WebImportContinuationRequest>(request).await {
        Ok(request) => request,
        Err(response) => return response,
    };
    let canonical = match canonical_continuation_request(conversation, request) {
        Ok(request) => request,
        Err(message) => return invalid_request(message),
    };
    execute_continuation(&state, canonical).await
}

struct CanonicalContinuationRequest {
    command_id: DurableCommandId,
    conversation: ImportedConversationId,
    entry: ImportedTranscriptEntryId,
    position: ImportedTranscriptPosition,
    relationship: ImportedSessionRelationship,
    web_relationship: WebImportedSessionRelationship,
    model_selection: ModelSelectionRequest,
}

fn canonical_continuation_request(
    path_conversation: ImportedConversationId,
    request: WebImportContinuationRequest,
) -> Result<CanonicalContinuationRequest, &'static str> {
    let command_id = required_uuid(&request.command_id)
        .map(DurableCommandId::from_uuid)
        .map_err(|_| "continuation command identity is not a UUID")?;
    let frontier_conversation =
        imported_conversation_id(&request.frontier.imported_conversation_id)?;
    if frontier_conversation != path_conversation {
        return Err("continuation frontier belongs to another import");
    }
    let position = canonical_positive_u64(&request.frontier.position)
        .and_then(ImportedTranscriptPosition::try_from_u64)
        .ok_or("continuation position must be positive")?;
    let entry = required_uuid(&request.frontier.imported_entry_id)
        .map(ImportedTranscriptEntryId::from_uuid)
        .map_err(|_| "continuation imported-entry identity is not a UUID")?;
    let model_selection = match request.initial_model_selection {
        WebModelSelection::Direct { selection_id } => {
            ModelSelectionRequest::Direct(DirectModelSelection::from_uuid(
                required_uuid(&selection_id)
                    .map_err(|_| "direct model selection identity is not a UUID")?,
            ))
        }
        WebModelSelection::Alias { alias_id } => {
            ModelSelectionRequest::Alias(ModelAlias::from_uuid(
                required_uuid(&alias_id).map_err(|_| "model alias identity is not a UUID")?,
            ))
        }
    };
    let relationship = domain_relationship(request.relationship);
    Ok(CanonicalContinuationRequest {
        command_id,
        conversation: path_conversation,
        entry,
        position,
        relationship,
        web_relationship: request.relationship,
        model_selection,
    })
}

async fn execute_continuation(
    state: &WebImportState,
    request: CanonicalContinuationRequest,
) -> Response {
    let repository = ImportedSessionRepository::new(
        state.pool.clone(),
        state.model_configuration.session_credential_pin(),
    );
    match repository.load(request.command_id).await {
        Ok(Some(recorded)) => {
            let command = recorded.command();
            if command.imported_conversation() != request.conversation
                || command.imported_frontier().through_entry() != request.entry
                || command.imported_frontier().through_position() != request.position
                || command.relationship() != request.relationship
                || command.initial_configuration_defaults().model() != request.model_selection
            {
                return conflicting_reuse();
            }
            return continuation_response(request, recorded.applied_result().session().into_uuid());
        }
        Ok(None) => {}
        Err(ImportedSessionRepositoryError::DifferentCommandKind { .. }) => {
            return conflicting_reuse();
        }
        Err(error) => return imported_session_error(error),
    }
    let frontier = ImportedTranscriptFrontier::from_parts(
        request.conversation,
        request.entry,
        request.position,
    );
    if state
        .model_configuration
        .resolve_session_model(request.model_selection)
        .is_err()
    {
        // The request passed transport decoding; rejecting an unconfigured model is a
        // state-dependent application decision, not a trust-boundary failure.
        return application_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "model_selection_not_configured",
            "initial model selection is not configured",
        );
    }
    let application_request = match CreateSessionFromImportedFrontierRequest::try_new(
        request.command_id,
        frontier,
        request.relationship,
        SessionConfigurationDefaults::new(request.model_selection),
    ) {
        Ok(request) => request,
        Err(_) => return invalid_request("continuation command identity is reserved"),
    };
    let mut service = CreateSessionFromImportedFrontierService::new(
        UuidV7CreateSessionFromImportedFrontierIdGenerator,
        repository,
    );
    match service.execute(application_request).await {
        Ok(CreateSessionFromImportedFrontierOutcome::Applied(result)) => {
            continuation_response(request, result.session().into_uuid())
        }
        Ok(CreateSessionFromImportedFrontierOutcome::ImportedConversationNotFound { .. }) => {
            import_not_found()
        }
        Ok(CreateSessionFromImportedFrontierOutcome::ImportedFrontierNotFound { .. }) => {
            // The frontier decoded successfully and failed only against durable application
            // state, so this is an application rejection, not a trust-boundary failure.
            application_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "import_frontier_not_found",
                "selected imported frontier no longer resolves",
            )
        }
        Ok(CreateSessionFromImportedFrontierOutcome::ConflictingReuse { .. }) => {
            conflicting_reuse()
        }
        Err(error) => imported_session_error(error),
    }
}

fn web_summary(summary: ImportedConversationSummary) -> WebImportSummary {
    WebImportSummary {
        imported_conversation_id: summary.conversation.into_uuid().to_string(),
        display_title: summary.display_title.map(|title| title.into_string()),
        format: web_format(summary.format),
        source_session_id: summary.source_session_id.map(web_source_session),
        entry_count: summary.entry_count.to_string(),
    }
}

fn web_descriptor(descriptor: ImportedConversationDescriptor) -> WebImportDescriptor {
    WebImportDescriptor {
        imported_conversation_id: descriptor.conversation.into_uuid().to_string(),
        display_title: descriptor.display_title.map(|title| title.into_string()),
        raw_record_count: descriptor.raw_record_count.to_string(),
        entry_count: descriptor.entry_count.to_string(),
        source: WebImportSourceEvidence {
            format: web_format(descriptor.format),
            source_digest_sha256: lowercase_hex(&descriptor.source_digest),
            source_session_id: descriptor.source_session_id.map(web_source_session),
        },
        sizes: WebImportSizeFacts {
            raw_source_bytes: descriptor.sizes.raw_source_bytes.to_string(),
            normalized_source_record_bytes: descriptor
                .sizes
                .normalized_source_record_bytes
                .to_string(),
            normalized_entry_bytes: descriptor.sizes.normalized_entry_bytes.to_string(),
        },
        timeline: WebImportTimelineBounds {
            first: web_frontier(descriptor.first),
            latest: web_frontier(descriptor.latest),
        },
    }
}

fn web_entry_window(window: ImportedEntryWindow) -> WebImportEntryWindow {
    WebImportEntryWindow {
        anchor_position: window.anchor_position.to_string(),
        first_position: window.first_position.to_string(),
        last_position: window.last_position.to_string(),
        has_before: window.has_before,
        has_after: window.has_after,
        items: window.items.into_iter().map(web_entry).collect(),
    }
}

fn web_entry(entry: ImportedEntryProjection) -> WebImportedEntry {
    let (content_kind, text) = web_content(&entry.content);
    WebImportedEntry {
        frontier: web_frontier(entry.frontier),
        raw_record_position: entry.raw_record_position.to_string(),
        record_entry_position: entry.record_entry_position.to_string(),
        source_speaker: match entry.source_speaker {
            ImportedSourceAttestation::NotAttested => WebImportedSpeakerEvidence::NotAttested,
            ImportedSourceAttestation::AttestedAbsent => WebImportedSpeakerEvidence::AttestedAbsent,
            ImportedSourceAttestation::Attested(ImportedSpeaker::User) => {
                WebImportedSpeakerEvidence::User
            }
            ImportedSourceAttestation::Attested(ImportedSpeaker::Assistant) => {
                WebImportedSpeakerEvidence::Assistant
            }
        },
        content_kind,
        text,
    }
}

fn web_content(
    content: &ImportedEntryContentProjection,
) -> (WebImportedContentKind, Option<WebImportTextEvidence>) {
    match content {
        ImportedEntryContentProjection::SourceEvent => (WebImportedContentKind::SourceEvent, None),
        ImportedEntryContentProjection::SourceMessageBlock => {
            (WebImportedContentKind::SourceMessageBlock, None)
        }
        ImportedEntryContentProjection::Text(text) => (
            WebImportedContentKind::Text,
            Some(match text {
                ImportedSourceAttestation::NotAttested => WebImportTextEvidence::NotAttested,
                ImportedSourceAttestation::AttestedAbsent => WebImportTextEvidence::AttestedAbsent,
                ImportedSourceAttestation::Attested(text) => WebImportTextEvidence::Attested {
                    leading_text: text.leading_text.clone(),
                    completeness: web_completeness(text.complete),
                },
            }),
        ),
        ImportedEntryContentProjection::ToolCall => (WebImportedContentKind::ToolCall, None),
        ImportedEntryContentProjection::ToolResult => (WebImportedContentKind::ToolResult, None),
        ImportedEntryContentProjection::Thinking => (WebImportedContentKind::Thinking, None),
        ImportedEntryContentProjection::RedactedThinking => {
            (WebImportedContentKind::RedactedThinking, None)
        }
        ImportedEntryContentProjection::Document => (WebImportedContentKind::Document, None),
        ImportedEntryContentProjection::MessageContentAbsent => {
            (WebImportedContentKind::MessageContentAbsent, None)
        }
    }
}

fn web_source_session(source_session_id: ImportedTextProjection) -> WebImportSourceSessionEvidence {
    WebImportSourceSessionEvidence {
        leading_text: source_session_id.leading_text,
        completeness: web_completeness(source_session_id.complete),
    }
}

fn web_completeness(complete: bool) -> WebImportTextCompleteness {
    if complete {
        WebImportTextCompleteness::Complete
    } else {
        WebImportTextCompleteness::Truncated
    }
}

fn web_frontier(frontier: ImportedContinuationReference) -> WebImportContinuationReference {
    WebImportContinuationReference {
        imported_conversation_id: frontier.conversation.into_uuid().to_string(),
        imported_entry_id: frontier.entry.into_uuid().to_string(),
        position: frontier.position.to_string(),
    }
}

fn continuation_response(request: CanonicalContinuationRequest, session: Uuid) -> Response {
    // The receipt spells every identity from the canonical domain values, never from the
    // caller's request text: `Uuid::parse_str` admits noncanonical spellings the generated
    // browser contract would refuse to decode, and an undecodable receipt for a committed
    // command would make every exact replay fail the same way.
    Json(WebImportContinuationResponse {
        command_id: request.command_id.as_uuid().to_string(),
        session_id: session.to_string(),
        frontier: WebImportContinuationReference {
            imported_conversation_id: request.conversation.into_uuid().to_string(),
            imported_entry_id: request.entry.into_uuid().to_string(),
            position: request.position.as_u64().to_string(),
        },
        relationship: request.web_relationship,
    })
    .into_response()
}

fn web_window_anchor(
    anchor: Option<WebImportWindowAnchor>,
    position: Option<String>,
) -> Result<ImportedEntryWindowAnchor, &'static str> {
    match (anchor.unwrap_or(WebImportWindowAnchor::First), position) {
        (WebImportWindowAnchor::First, None) => Ok(ImportedEntryWindowAnchor::First),
        (WebImportWindowAnchor::Latest, None) => Ok(ImportedEntryWindowAnchor::Latest),
        (WebImportWindowAnchor::Position, Some(position)) => canonical_positive_u64(&position)
            .map(ImportedEntryWindowAnchor::Position)
            .ok_or("entry-window position must be a positive canonical decimal u64"),
        _ => Err("entry-window position is present exactly for the position anchor"),
    }
}

fn canonical_positive_u64(value: &str) -> Option<u64> {
    if value.starts_with('0') || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok().filter(|value| *value > 0)
}

fn domain_format(format: WebImportFormat) -> ImportedConversationFormat {
    match format {
        WebImportFormat::ClaudeCodeSessionJsonlV1 => {
            ImportedConversationFormat::ClaudeCodeSessionJsonlV1
        }
        WebImportFormat::ClaudeCodeSessionJsonlV2 => {
            ImportedConversationFormat::ClaudeCodeSessionJsonlV2
        }
        WebImportFormat::CodexRolloutJsonlV1 => ImportedConversationFormat::CodexRolloutJsonlV1,
    }
}

fn web_format(format: ImportedConversationFormat) -> WebImportFormat {
    match format {
        ImportedConversationFormat::ClaudeCodeSessionJsonlV1 => {
            WebImportFormat::ClaudeCodeSessionJsonlV1
        }
        ImportedConversationFormat::ClaudeCodeSessionJsonlV2 => {
            WebImportFormat::ClaudeCodeSessionJsonlV2
        }
        ImportedConversationFormat::CodexRolloutJsonlV1 => WebImportFormat::CodexRolloutJsonlV1,
    }
}

fn domain_relationship(
    relationship: WebImportedSessionRelationship,
) -> ImportedSessionRelationship {
    match relationship {
        WebImportedSessionRelationship::Resume => ImportedSessionRelationship::Resume,
        WebImportedSessionRelationship::Fork => ImportedSessionRelationship::Fork,
    }
}

fn lowercase_hex(bytes: &[u8]) -> String {
    const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(char::from(HEX_DIGITS[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX_DIGITS[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn optional_uuid(value: Option<&str>) -> Result<Option<Uuid>, &'static str> {
    value
        .map(required_uuid)
        .transpose()
        .map_err(|_| "imports cursor is not an imported-conversation UUID")
}

fn required_uuid(value: &str) -> Result<Uuid, uuid::Error> {
    Uuid::parse_str(value)
}

fn imported_conversation_id(value: &str) -> Result<ImportedConversationId, &'static str> {
    required_uuid(value)
        .map(ImportedConversationId::from_uuid)
        .map_err(|_| "imported-conversation identity is not a UUID")
}

fn discovery_error(error: ImportedConversationDiscoveryError) -> Response {
    match error {
        ImportedConversationDiscoveryError::Database(_) => application_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "imports_unavailable",
            "imported-conversation discovery is temporarily unavailable",
        ),
        ImportedConversationDiscoveryError::Request(
            ImportedConversationDiscoveryRequestError::PositionOutOfRange,
        ) => position_out_of_range(),
        ImportedConversationDiscoveryError::Request(
            ImportedConversationDiscoveryRequestError::WindowTooLarge,
        ) => invalid_request("imported entry window exceeds the contract bound"),
        ImportedConversationDiscoveryError::Corruption(_) => application_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "import_projection_corrupt",
            "stored imported-conversation facts failed closed validation",
        ),
    }
}

fn imported_session_error(error: ImportedSessionRepositoryError) -> Response {
    match error {
        ImportedSessionRepositoryError::CommitAmbiguous(_) => application_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "continuation_commit_ambiguous",
            "continuation acknowledgement is ambiguous; retry the exact command",
        ),
        ImportedSessionRepositoryError::Database(_) => application_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "continuation_unavailable",
            "imported continuation is temporarily unavailable",
        ),
        ImportedSessionRepositoryError::DifferentCommandKind { .. } => conflicting_reuse(),
        ImportedSessionRepositoryError::Preparation(_)
        | ImportedSessionRepositoryError::IdentityCollision(_)
        | ImportedSessionRepositoryError::Corruption(_) => application_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "continuation_corrupt",
            "stored imported continuation failed closed validation",
        ),
    }
}

fn source_session_maximum_bytes() -> Option<NonZeroU32> {
    u32::try_from(MAX_IMPORT_SOURCE_SESSION_BYTES)
        .ok()
        .and_then(NonZeroU32::new)
}

fn invalid_import_contract() -> Response {
    application_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "invalid_import_contract",
        "imported discovery contract is invalid",
    )
}

fn invalid_request(message: &'static str) -> Response {
    transport_error(StatusCode::BAD_REQUEST, "invalid_import_request", message)
}

// A syntactically valid position that exceeds the stored entry count is rejected only after
// reading current application state, so it is an application decision, not a transport one.
fn position_out_of_range() -> Response {
    application_error(
        StatusCode::UNPROCESSABLE_ENTITY,
        "import_position_out_of_range",
        "imported entry-window position is outside the timeline",
    )
}

fn import_not_found() -> Response {
    application_error(
        StatusCode::NOT_FOUND,
        "import_not_found",
        "imported conversation does not exist",
    )
}

fn conflicting_reuse() -> Response {
    application_error(
        StatusCode::CONFLICT,
        "conflicting_command_reuse",
        "durable command identity already names another payload",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imported_text_projection_preserves_truncated_utf8_evidence() {
        let expected = "€".repeat(MAX_IMPORT_TEXT_PREVIEW_BYTES / "€".len());
        let content = ImportedEntryContentProjection::Text(ImportedSourceAttestation::Attested(
            ImportedTextProjection {
                leading_text: expected.clone(),
                complete: false,
            },
        ));

        assert_eq!(
            web_content(&content),
            (
                WebImportedContentKind::Text,
                Some(WebImportTextEvidence::Attested {
                    leading_text: expected,
                    completeness: WebImportTextCompleteness::Truncated,
                }),
            )
        );
    }

    #[test]
    fn imported_text_projection_marks_a_complete_value() {
        let source = "complete imported text";
        let content = ImportedEntryContentProjection::Text(ImportedSourceAttestation::Attested(
            ImportedTextProjection {
                leading_text: source.to_owned(),
                complete: true,
            },
        ));

        assert_eq!(
            web_content(&content),
            (
                WebImportedContentKind::Text,
                Some(WebImportTextEvidence::Attested {
                    leading_text: source.to_owned(),
                    completeness: WebImportTextCompleteness::Complete,
                }),
            )
        );
    }

    #[test]
    fn source_session_projection_preserves_bounded_evidence() {
        let leading_text = "€".repeat(MAX_IMPORT_SOURCE_SESSION_BYTES / "€".len());
        let evidence = web_source_session(ImportedTextProjection {
            leading_text: leading_text.clone(),
            complete: false,
        });

        assert_eq!(evidence.leading_text, leading_text);
        assert_eq!(evidence.completeness, WebImportTextCompleteness::Truncated);
    }

    #[test]
    fn position_anchor_preserves_its_exact_position() {
        assert_eq!(
            web_window_anchor(Some(WebImportWindowAnchor::Position), Some("7".to_owned())),
            Ok(ImportedEntryWindowAnchor::Position(7))
        );
    }

    #[test]
    fn position_anchor_rejects_an_absent_position() {
        assert_eq!(
            web_window_anchor(Some(WebImportWindowAnchor::Position), None),
            Err("entry-window position is present exactly for the position anchor")
        );
    }

    #[test]
    fn non_position_anchor_rejects_a_supplied_position() {
        assert_eq!(
            web_window_anchor(Some(WebImportWindowAnchor::Latest), Some("7".to_owned())),
            Err("entry-window position is present exactly for the position anchor")
        );
    }
}

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
use sha2::Digest;
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
    conversation_import::{
        ImportedConversationRepository, ImportedConversationRepositoryError,
        ImportedRawBlobStorageError,
    },
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
    BlobStoreRegistry, HubModelConfiguration,
    imported_source_blobs::ImportedSourceBlobStorage,
    web_http::{
        application_error, decode_bounded_json, decode_bounded_utf8, transport_error,
        validate_json_mutation, validate_text_mutation,
    },
};

// Tunable effective ceiling: ordinary catalog views ask for a dense first page below the hard cap.
const DEFAULT_IMPORT_LIST_ITEMS: u32 = 50;
// Tunable effective ceiling: default entry windows retain 25 neighbors on each side.
const DEFAULT_IMPORT_WINDOW_RADIUS: u32 = 25;

#[derive(Clone, Debug)]
struct WebImportState {
    pool: PgPool,
    model_configuration: HubModelConfiguration,
    imported_conversations: ImportedConversationRepository,
}

pub(crate) fn router(
    pool: PgPool,
    model_configuration: HubModelConfiguration,
    blob_store_registry: Option<Arc<BlobStoreRegistry>>,
) -> Router {
    // Continuations reconstitute the imported aggregate from immutable blob
    // storage, so the browser adapter shares the daemon's publication adapter
    // rather than reading raw bytes the database no longer holds.
    let imported_conversations = ImportedConversationRepository::with_blob_storage(
        pool.clone(),
        Arc::new(ImportedSourceBlobStorage::new(
            pool.clone(),
            blob_store_registry,
            model_configuration.conversation_import_max_source_bytes(),
        )),
    );
    let state = WebImportState {
        pool,
        model_configuration,
        imported_conversations,
    };
    let mutation = Router::new()
        .route("/{conversation}/continuations", post(continue_import))
        .route_layer(middleware::from_fn(validate_json_mutation));
    let searches = Router::new()
        .route("/searches", post(search_imports))
        .route_layer(middleware::from_fn(validate_text_mutation));
    Router::new()
        .route("/", get(list_imports))
        .route("/{conversation}", get(read_descriptor))
        .route("/{conversation}/entries", get(read_entry_window))
        .merge(searches)
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
    if request.source_session_id.is_some() || request.search_correlation.is_some() {
        return invalid_request("exact source-session filters use the bounded search body");
    }
    execute_list_imports(state, request, None).await
}

async fn search_imports(
    State(state): State<WebImportState>,
    query: Result<Query<WebImportListRequest>, axum::extract::rejection::QueryRejection>,
    request: axum::extract::Request,
) -> Response {
    let Query(mut catalog_request) = match query {
        Ok(request) => request,
        Err(_) => return invalid_request("imports search query is malformed"),
    };
    if catalog_request.source_session_id.is_some() {
        return invalid_request("exact source-session filters belong in the search body");
    }
    let search_correlation = match catalog_request.search_correlation.as_deref() {
        Some(value) if required_uuid(value).is_ok() => Some(value.to_owned()),
        Some(_) => return invalid_request("imports search correlation is not a UUID"),
        None => return invalid_request("imports search correlation is required"),
    };
    let maximum_bytes = state
        .model_configuration
        .conversation_import_max_source_bytes();
    let source_session_id = match decode_bounded_utf8(request, maximum_bytes).await {
        Ok(source_session_id) => source_session_id,
        Err(response) => return response,
    };
    catalog_request.source_session_id = Some(source_session_id);
    execute_list_imports(state, catalog_request, search_correlation).await
}

async fn execute_list_imports(
    state: WebImportState,
    request: WebImportListRequest,
    search_correlation: Option<String>,
) -> Response {
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
    let exact_source_session_id_sha256 = request
        .source_session_id
        .as_deref()
        .map(|value| hex::encode(sha2::Sha256::digest(value.as_bytes())));
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
            search_correlation,
            exact_source_session_id_sha256,
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
        )) => import_position_out_of_range(),
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
    web_frontier: WebImportContinuationReference,
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
    let position = ImportedTranscriptPosition::try_from_u64(request.frontier.position)
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
        web_frontier: request.frontier,
    })
}

async fn execute_continuation(
    state: &WebImportState,
    request: CanonicalContinuationRequest,
) -> Response {
    let repository = ImportedSessionRepository::with_imported_conversations(
        state.pool.clone(),
        state.model_configuration.session_credential_pin(),
        state.imported_conversations.clone(),
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
        return application_error(
            StatusCode::BAD_REQUEST,
            "model_not_configured",
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
            application_error(
                StatusCode::BAD_REQUEST,
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
        source_session_id_sha256: summary.source_session_digest.as_ref().map(hex::encode),
        entry_count: summary.entry_count,
    }
}

fn web_descriptor(descriptor: ImportedConversationDescriptor) -> WebImportDescriptor {
    WebImportDescriptor {
        imported_conversation_id: descriptor.conversation.into_uuid().to_string(),
        display_title: descriptor.display_title.map(|title| title.into_string()),
        raw_record_count: descriptor.raw_record_count,
        entry_count: descriptor.entry_count,
        source: WebImportSourceEvidence {
            format: web_format(descriptor.format),
            source_digest_sha256: hex::encode(descriptor.source_digest),
            source_session_id: descriptor.source_session_id.map(web_source_session),
        },
        sizes: WebImportSizeFacts {
            raw_source_bytes: descriptor.sizes.raw_source_bytes,
            normalized_source_record_bytes: descriptor.sizes.normalized_source_record_bytes,
            normalized_entry_bytes: descriptor.sizes.normalized_entry_bytes,
        },
        timeline: WebImportTimelineBounds {
            first: web_frontier(descriptor.first),
            latest: web_frontier(descriptor.latest),
        },
    }
}

fn web_entry_window(window: ImportedEntryWindow) -> WebImportEntryWindow {
    WebImportEntryWindow {
        anchor_position: window.anchor_position,
        first_position: window.first_position,
        last_position: window.last_position,
        has_before: window.has_before,
        has_after: window.has_after,
        items: window.items.into_iter().map(web_entry).collect(),
    }
}

fn web_entry(entry: ImportedEntryProjection) -> WebImportedEntry {
    let (content_kind, text) = web_content(&entry.content);
    WebImportedEntry {
        frontier: web_frontier(entry.frontier),
        raw_record_position: entry.raw_record_position,
        record_entry_position: entry.record_entry_position,
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
        position: frontier.position,
    }
}

fn continuation_response(request: CanonicalContinuationRequest, session: Uuid) -> Response {
    Json(WebImportContinuationResponse {
        command_id: request.command_id.as_uuid().to_string(),
        session_id: session.to_string(),
        frontier: request.web_frontier,
        relationship: request.web_relationship,
    })
    .into_response()
}

fn web_window_anchor(
    anchor: Option<WebImportWindowAnchor>,
    position: Option<u64>,
) -> Result<ImportedEntryWindowAnchor, &'static str> {
    match (anchor.unwrap_or(WebImportWindowAnchor::First), position) {
        (WebImportWindowAnchor::First, None) => Ok(ImportedEntryWindowAnchor::First),
        (WebImportWindowAnchor::Latest, None) => Ok(ImportedEntryWindowAnchor::Latest),
        (WebImportWindowAnchor::Position, Some(position)) => {
            Ok(ImportedEntryWindowAnchor::Position(position))
        }
        _ => Err("entry-window position is present exactly for the position anchor"),
    }
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
        ) => import_position_out_of_range(),
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
        ImportedSessionRepositoryError::Database(_)
        | ImportedSessionRepositoryError::ImportedConversation(
            ImportedConversationRepositoryError::Database(_)
            | ImportedConversationRepositoryError::BlobStorage(
                ImportedRawBlobStorageError::Unavailable,
            ),
        ) => application_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "continuation_unavailable",
            "imported continuation is temporarily unavailable",
        ),
        ImportedSessionRepositoryError::DifferentCommandKind { .. } => conflicting_reuse(),
        ImportedSessionRepositoryError::Preparation(_)
        | ImportedSessionRepositoryError::IdentityCollision(_)
        | ImportedSessionRepositoryError::ImportedConversation(_)
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

fn import_not_found() -> Response {
    application_error(
        StatusCode::NOT_FOUND,
        "import_not_found",
        "imported conversation does not exist",
    )
}

fn import_position_out_of_range() -> Response {
    application_error(
        StatusCode::BAD_REQUEST,
        "import_position_out_of_range",
        "imported entry-window position is outside the timeline",
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

    const CONVERSATION_ID: &str = "00000000-0000-7000-8000-000000000001";
    const OTHER_CONVERSATION_ID: &str = "00000000-0000-7000-8000-000000000002";
    const ENTRY_ID: &str = "00000000-0000-7000-8000-000000000003";
    const COMMAND_ID: &str = "00000000-0000-7000-8000-000000000004";
    const MODEL_ID: &str = "00000000-0000-7000-8000-000000000005";

    fn continuation_request() -> WebImportContinuationRequest {
        WebImportContinuationRequest {
            command_id: COMMAND_ID.to_owned(),
            frontier: WebImportContinuationReference {
                imported_conversation_id: CONVERSATION_ID.to_owned(),
                imported_entry_id: ENTRY_ID.to_owned(),
                position: 1,
            },
            relationship: WebImportedSessionRelationship::Resume,
            initial_model_selection: WebModelSelection::Direct {
                selection_id: MODEL_ID.to_owned(),
            },
        }
    }

    fn conversation_id() -> ImportedConversationId {
        ImportedConversationId::from_uuid(
            Uuid::parse_str(CONVERSATION_ID).expect("fixture UUID is valid"),
        )
    }

    #[test]
    fn canonical_continuation_accepts_a_correlated_request() {
        assert!(canonical_continuation_request(conversation_id(), continuation_request()).is_ok());
    }

    #[test]
    fn canonical_continuation_rejects_a_non_uuid_command() {
        let mut request = continuation_request();
        request.command_id = "not-a-uuid".to_owned();

        assert_eq!(
            canonical_continuation_request(conversation_id(), request).err(),
            Some("continuation command identity is not a UUID")
        );
    }

    #[test]
    fn canonical_continuation_rejects_another_import() {
        let mut request = continuation_request();
        request.frontier.imported_conversation_id = OTHER_CONVERSATION_ID.to_owned();

        assert_eq!(
            canonical_continuation_request(conversation_id(), request).err(),
            Some("continuation frontier belongs to another import")
        );
    }

    #[test]
    fn canonical_continuation_rejects_zero_position() {
        let mut request = continuation_request();
        request.frontier.position = 0;

        assert_eq!(
            canonical_continuation_request(conversation_id(), request).err(),
            Some("continuation position must be positive")
        );
    }

    #[test]
    fn canonical_continuation_rejects_a_non_uuid_entry() {
        let mut request = continuation_request();
        request.frontier.imported_entry_id = "not-a-uuid".to_owned();

        assert_eq!(
            canonical_continuation_request(conversation_id(), request).err(),
            Some("continuation imported-entry identity is not a UUID")
        );
    }

    #[test]
    fn canonical_continuation_rejects_a_non_uuid_direct_model() {
        let mut request = continuation_request();
        request.initial_model_selection = WebModelSelection::Direct {
            selection_id: "not-a-uuid".to_owned(),
        };

        assert_eq!(
            canonical_continuation_request(conversation_id(), request).err(),
            Some("direct model selection identity is not a UUID")
        );
    }

    #[test]
    fn canonical_continuation_rejects_a_non_uuid_alias() {
        let mut request = continuation_request();
        request.initial_model_selection = WebModelSelection::Alias {
            alias_id: "not-a-uuid".to_owned(),
        };

        assert_eq!(
            canonical_continuation_request(conversation_id(), request).err(),
            Some("model alias identity is not a UUID")
        );
    }

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
            web_window_anchor(Some(WebImportWindowAnchor::Position), Some(7)),
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
            web_window_anchor(Some(WebImportWindowAnchor::Latest), Some(7)),
            Err("entry-window position is present exactly for the position anchor")
        );
    }

    #[tokio::test]
    async fn out_of_range_position_is_an_application_error() {
        let response = import_position_out_of_range();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), 4_096)
            .await
            .expect("the bounded error body is readable");
        let decoded: serde_json::Value =
            serde_json::from_slice(&body).expect("the application error is JSON");

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(decoded["error"]["kind"], "application");
        assert_eq!(decoded["error"]["code"], "import_position_out_of_range");
    }
}

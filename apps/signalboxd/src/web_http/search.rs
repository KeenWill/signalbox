use super::{
    Deserialize, IntoResponse, Json, Query, QueryRejection, Response, SearchContentClass,
    SearchCursor, SearchPageLimit, SearchQuery, SearchRepositoryError, SearchResultSource,
    SearchScope, SearchStrategy, SearchText, State, StatusCode, TimelineAddress, WebApiState,
    WebSearchContentClass, WebSearchCursor, WebSearchHighlight, WebSearchPage,
    WebSearchProjectionId, WebSearchResult, WebSearchResultSource, WebSessionId, WebUuid,
    address_dto, application_error, parse_session_id,
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SearchHttpQuery {
    strategy: String,
    q: String,
    session_id: Option<String>,
    max_items: String,
    after_address: Option<String>,
    after_projection: Option<String>,
}

pub(super) async fn search(
    State(state): State<WebApiState>,
    query: Result<Query<SearchHttpQuery>, QueryRejection>,
) -> Response {
    let Query(query) = match query {
        Ok(query) => query,
        Err(_) => return invalid_search_query(),
    };
    let Some(request) = parse_search_query(query) else {
        return invalid_search_query();
    };
    let Some(repository) = state.search else {
        return application_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "search_projection_unavailable",
            "search projection is not configured",
        );
    };
    // The lexical read holds one pooled connection across `SET TRANSACTION`,
    // the term probe, and the page query, so it is a snapshot reader on the
    // same footing as the attention snapshot: it draws its permit from the
    // daemon-wide budget that reserves pool connections for mutations and
    // outbox work. Admitting it after the query and repository checks keeps a
    // malformed or unconfigured request from spending a permit.
    let Some(budget) = state.snapshot_reader_budget else {
        return search_projection_failed();
    };
    let Ok(_permit) = budget.acquire().await else {
        return search_projection_failed();
    };
    match repository.search(request).await {
        Ok(page) => Json(search_page_dto(page)).into_response(),
        Err(error) => search_repository_error(error),
    }
}

fn parse_search_query(query: SearchHttpQuery) -> Option<SearchQuery> {
    if query.strategy != "lexical" {
        return None;
    }
    let text = SearchText::try_new(query.q).ok()?;
    let limit = query
        .max_items
        .parse::<u16>()
        .ok()
        .and_then(|value| SearchPageLimit::new(value).ok())?;
    let scope = match query.session_id {
        Some(value) => SearchScope::Session(parse_session_id(&value).ok()?),
        None => SearchScope::Global,
    };
    let after = match (query.after_address, query.after_projection) {
        (None, None) => None,
        (Some(address), Some(projection)) => Some(SearchCursor::new(
            TimelineAddress::new(parse_positive_u64(&address)?),
            parse_positive_i64(&projection)?,
        )),
        _ => return None,
    };
    Some(SearchQuery {
        strategy: SearchStrategy::Lexical,
        scope,
        text,
        limit,
        after,
    })
}

fn parse_positive_u64(value: &str) -> Option<std::num::NonZeroU64> {
    value
        .parse::<u64>()
        .ok()
        .and_then(std::num::NonZeroU64::new)
}

fn parse_positive_i64(value: &str) -> Option<std::num::NonZeroU64> {
    value
        .parse::<i64>()
        .ok()
        .and_then(|value| u64::try_from(value).ok())
        .and_then(std::num::NonZeroU64::new)
}

fn invalid_search_query() -> Response {
    application_error(
        StatusCode::BAD_REQUEST,
        "invalid_search_query",
        "search parameters are malformed or outside the contract bounds",
    )
}

fn search_repository_error(error: SearchRepositoryError) -> Response {
    let failure_class = match &error {
        SearchRepositoryError::Database(_) => "infrastructure",
        SearchRepositoryError::Corruption(_) => "fail_closed_corruption",
    };
    tracing::error!(failure_class, cause = %error, "lexical search projection read failed");
    search_projection_failed()
}

fn search_projection_failed() -> Response {
    application_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "search_projection_failed",
        "the durable search projection could not be read",
    )
}

fn search_page_dto(page: signalbox_application::SearchPage) -> WebSearchPage {
    WebSearchPage {
        results: page.results.into_iter().map(search_result_dto).collect(),
        continuation: page.next.map(|cursor| WebSearchCursor {
            address: address_dto(cursor.address()),
            projection_id: WebSearchProjectionId::from_nonzero(cursor.projection()),
        }),
    }
}

fn search_result_dto(result: signalbox_application::SearchResult) -> WebSearchResult {
    WebSearchResult {
        session_id: WebSessionId::from_validated_uuid(result.session.into_uuid().to_string()),
        address: address_dto(result.address),
        projection_id: WebSearchProjectionId::from_nonzero(result.projection),
        source: search_source_dto(result.source),
        content_class: search_content_class_dto(result.content_class),
        snippet: result.snippet,
        highlights: result
            .highlights
            .into_iter()
            .map(|highlight| WebSearchHighlight {
                start_byte: u32::from(highlight.start_byte),
                end_byte: u32::from(highlight.end_byte),
            })
            .collect(),
    }
}

fn search_source_dto(source: SearchResultSource) -> WebSearchResultSource {
    match source {
        SearchResultSource::Session(session) => WebSearchResultSource::Session {
            session_id: WebSessionId::from_validated_uuid(session.into_uuid().to_string()),
        },
        SearchResultSource::AcceptedInput { input, turn } => WebSearchResultSource::AcceptedInput {
            accepted_input_id: web_uuid(input.into_uuid()),
            turn_id: web_uuid(turn.into_uuid()),
        },
        SearchResultSource::SteeringInput { input, source_turn } => {
            WebSearchResultSource::SteeringInput {
                accepted_input_id: web_uuid(input.into_uuid()),
                source_turn_id: web_uuid(source_turn.into_uuid()),
            }
        }
        SearchResultSource::TurnTranscriptEntry { entry, turn } => {
            WebSearchResultSource::TurnTranscriptEntry {
                semantic_entry_id: web_uuid(entry.into_uuid()),
                turn_id: web_uuid(turn.into_uuid()),
            }
        }
        SearchResultSource::SessionTranscriptEntry { entry } => {
            WebSearchResultSource::SessionTranscriptEntry {
                semantic_entry_id: web_uuid(entry.into_uuid()),
            }
        }
        SearchResultSource::ToolRequest { request, turn } => WebSearchResultSource::ToolRequest {
            tool_request_id: web_uuid(request.into_uuid()),
            turn_id: web_uuid(turn.into_uuid()),
        },
        SearchResultSource::ToolAttempt { attempt, turn } => WebSearchResultSource::ToolAttempt {
            tool_attempt_id: web_uuid(attempt.into_uuid()),
            turn_id: web_uuid(turn.into_uuid()),
        },
        SearchResultSource::Attachment { attachment } => WebSearchResultSource::Attachment {
            attachment_id: web_uuid(attachment.into_uuid()),
        },
        SearchResultSource::DerivedArtifact { artifact } => {
            WebSearchResultSource::DerivedArtifact {
                artifact_id: web_uuid(artifact.into_uuid()),
            }
        }
    }
}

pub(super) fn web_uuid(value: uuid::Uuid) -> WebUuid {
    WebUuid::from_validated_uuid(value.to_string())
}

fn search_content_class_dto(content: SearchContentClass) -> WebSearchContentClass {
    match content {
        SearchContentClass::UserTranscript => WebSearchContentClass::UserTranscript,
        SearchContentClass::AssistantTranscript => WebSearchContentClass::AssistantTranscript,
        SearchContentClass::ToolArguments => WebSearchContentClass::ToolArguments,
        SearchContentClass::ToolResult => WebSearchContentClass::ToolResult,
        SearchContentClass::SessionMetadata => WebSearchContentClass::SessionMetadata,
        SearchContentClass::AttachmentFilename => WebSearchContentClass::AttachmentFilename,
        SearchContentClass::AttachmentMediaMetadata => {
            WebSearchContentClass::AttachmentMediaMetadata
        }
        SearchContentClass::DerivedTextArtifact => WebSearchContentClass::DerivedTextArtifact,
    }
}

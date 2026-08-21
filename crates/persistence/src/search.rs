//! PostgreSQL adapter for bounded application lexical search.

use std::{error::Error, fmt, num::NonZeroU64};

use rust_decimal::Decimal;
use signalbox_application::{
    SearchArtifactId, SearchArtifactProjection, SearchArtifactProjectionClass, SearchContentClass,
    SearchCursor, SearchHighlight, SearchPage, SearchProjectionWriter, SearchQuery, SearchReader,
    SearchResult, SearchResultOwner, SearchScope, SearchStrategy, TimelineAddress,
    max_search_snippet_bytes,
};
use signalbox_domain::{
    AcceptedInputId, SemanticTranscriptEntryId, SessionId, ToolAttemptId, ToolRequestId, TurnId,
};
use sqlx::{PgPool, Row, postgres::PgRow};
use uuid::Uuid;

const HEADLINE_START: &str = "\u{e000}";
const HEADLINE_END: &str = "\u{e001}";

const SEARCH_SQL: &str = "
SELECT projection_id, session_id, event_sequence, owner_kind, owner_id,
       turn_id, content_class,
       left(
           ts_headline(
               'simple'::regconfig,
               replace(replace(content_text, chr(57344), ''), chr(57345), ''),
               plainto_tsquery('simple'::regconfig, $1),
               'StartSel=' || chr(57344) || ', StopSel=' || chr(57345) ||
               ', MaxWords=32, MinWords=8, ShortWord=1, MaxFragments=1'
           ),
           2048
       ) AS marked_snippet
  FROM web_search_projection
 WHERE search_vector @@ plainto_tsquery('simple'::regconfig, $1)
   AND ($2::uuid IS NULL OR session_id = $2)
   AND (
       $3::numeric IS NULL
       OR event_sequence < $3
       OR (event_sequence = $3 AND projection_id < $4)
   )
 ORDER BY event_sequence DESC, projection_id DESC
 LIMIT $5";

const PUBLISH_ARTIFACT_SQL: &str = "
INSERT INTO web_search_projection (
    source_kind, source_id, session_id, event_sequence,
    owner_kind, owner_id, turn_id, content_class, content_text
) VALUES ($1, $2, $3, $4, $1, $2, NULL, $5, $6)
ON CONFLICT (source_kind, source_id, content_class) DO UPDATE
       SET content_text = web_search_projection.content_text
     WHERE web_search_projection.session_id = EXCLUDED.session_id
       AND web_search_projection.event_sequence = EXCLUDED.event_sequence
       AND web_search_projection.owner_kind = EXCLUDED.owner_kind
       AND web_search_projection.owner_id = EXCLUDED.owner_id
       AND web_search_projection.turn_id IS NOT DISTINCT FROM EXCLUDED.turn_id
       AND web_search_projection.content_text = EXCLUDED.content_text
RETURNING projection_id";

/// Integrity failure in the dedicated search projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SearchProjectionCorruption {
    /// A required projected field was absent or malformed.
    Invalid(&'static str),
    /// A closed stored discriminator was unsupported.
    Unsupported {
        /// Projection field carrying the unsupported spelling.
        field: &'static str,
        /// Exact unsupported spelling.
        value: String,
    },
    /// Stored owner fields contradicted the selected typed owner.
    OwnerShape,
}

impl fmt::Display for SearchProjectionCorruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(field) => write!(formatter, "invalid search projection {field}"),
            Self::Unsupported { field, value } => {
                write!(formatter, "unsupported search projection {field}: {value}")
            }
            Self::OwnerShape => formatter.write_str("search projection owner shape is invalid"),
        }
    }
}

impl Error for SearchProjectionCorruption {}

/// Database or fail-closed lexical projection failure.
#[derive(Debug)]
pub enum SearchRepositoryError {
    /// PostgreSQL query failure.
    Database(sqlx::Error),
    /// Projection row violated the application representation.
    Corruption(SearchProjectionCorruption),
}

impl fmt::Display for SearchRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => write!(formatter, "search database failure: {error}"),
            Self::Corruption(error) => error.fmt(formatter),
        }
    }
}

impl Error for SearchRepositoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            Self::Corruption(error) => Some(error),
        }
    }
}

impl From<sqlx::Error> for SearchRepositoryError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

impl From<SearchProjectionCorruption> for SearchRepositoryError {
    fn from(error: SearchProjectionCorruption) -> Self {
        Self::Corruption(error)
    }
}

/// PostgreSQL implementation of the lexical search strategy.
#[derive(Clone, Debug)]
pub struct SearchRepository {
    pool: PgPool,
}

impl SearchRepository {
    /// Uses the supplied pool for indexed bounded reads.
    #[must_use]
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Executes one strict keyset page over the dedicated full-text projection.
    pub async fn search(&self, query: SearchQuery) -> Result<SearchPage, SearchRepositoryError> {
        match query.strategy {
            SearchStrategy::Lexical => {}
        }
        let session = match query.scope {
            SearchScope::Global => None,
            SearchScope::Session(session) => Some(session.into_uuid()),
        };
        let cursor_address = query
            .after
            .map(|cursor| Decimal::from(cursor.address().sequence().get()));
        let cursor_projection = query
            .after
            .map(|cursor| i64::try_from(cursor.projection().get()))
            .transpose()
            .map_err(|_| SearchProjectionCorruption::Invalid("cursor projection"))?;
        let fetch_limit = i64::from(query.limit.get()) + 1;
        let rows = sqlx::query(SEARCH_SQL)
            .bind(query.text.as_str())
            .bind(session)
            .bind(cursor_address)
            .bind(cursor_projection)
            .bind(fetch_limit)
            .fetch_all(&self.pool)
            .await?;
        decode_page(rows, usize::from(query.limit.get()))
    }

    /// Publishes text only when its owning durable artifact explicitly supplies it.
    pub async fn publish(
        &self,
        projection: SearchArtifactProjection,
    ) -> Result<(), SearchRepositoryError> {
        let (source_kind, content_class) = match projection.class {
            SearchArtifactProjectionClass::AttachmentFilename => {
                ("attachment", "attachment_filename")
            }
            SearchArtifactProjectionClass::AttachmentMediaMetadata => {
                ("attachment", "attachment_media_metadata")
            }
            SearchArtifactProjectionClass::DerivedText => {
                ("derived_artifact", "derived_text_artifact")
            }
        };
        let published = sqlx::query_scalar::<_, i64>(PUBLISH_ARTIFACT_SQL)
            .bind(source_kind)
            .bind(projection.artifact.into_uuid())
            .bind(projection.session.into_uuid())
            .bind(Decimal::from(projection.address.sequence().get()))
            .bind(content_class)
            .bind(projection.text.as_str())
            .fetch_optional(&self.pool)
            .await?;
        published.ok_or(SearchProjectionCorruption::Invalid(
            "conflicting artifact publication",
        ))?;
        Ok(())
    }
}

impl SearchReader for SearchRepository {
    type Error = SearchRepositoryError;

    async fn search(&self, query: SearchQuery) -> Result<SearchPage, Self::Error> {
        self.search(query).await
    }
}

impl SearchProjectionWriter for SearchRepository {
    type Error = SearchRepositoryError;

    async fn publish(&self, projection: SearchArtifactProjection) -> Result<(), Self::Error> {
        self.publish(projection).await
    }
}

fn decode_page(rows: Vec<PgRow>, limit: usize) -> Result<SearchPage, SearchRepositoryError> {
    let has_more = rows.len() > limit;
    let mut decoded = rows
        .into_iter()
        .take(limit)
        .map(decode_row)
        .collect::<Result<Vec<_>, _>>()?;
    let next = if has_more {
        decoded.last().map(|(cursor, _)| *cursor)
    } else {
        None
    };
    Ok(SearchPage {
        results: decoded.drain(..).map(|(_, result)| result).collect(),
        next,
    })
}

fn decode_row(row: PgRow) -> Result<(SearchCursor, SearchResult), SearchRepositoryError> {
    let projection_id: i64 = row.try_get("projection_id")?;
    let projection = u64::try_from(projection_id)
        .ok()
        .and_then(NonZeroU64::new)
        .ok_or(SearchProjectionCorruption::Invalid("projection identity"))?;
    let address = required_address(row.try_get("event_sequence")?)?;
    let session = SessionId::from_uuid(row.try_get("session_id")?);
    let owner_kind: String = row.try_get("owner_kind")?;
    let owner_id: Uuid = row.try_get("owner_id")?;
    let turn_id: Option<Uuid> = row.try_get("turn_id")?;
    let owner = decode_owner(&owner_kind, owner_id, turn_id, session)?;
    let content_class = decode_content_class(row.try_get("content_class")?)?;
    let (snippet, highlights) = decode_headline(row.try_get("marked_snippet")?)?;
    Ok((
        SearchCursor::new(address, projection),
        SearchResult {
            session,
            address,
            owner,
            content_class,
            snippet,
            highlights,
        },
    ))
}

fn decode_owner(
    kind: &str,
    owner: Uuid,
    turn: Option<Uuid>,
    session: SessionId,
) -> Result<SearchResultOwner, SearchProjectionCorruption> {
    match (kind, turn) {
        ("session", None) if owner == session.into_uuid() => {
            Ok(SearchResultOwner::Session(session))
        }
        ("accepted_input", Some(turn)) => Ok(SearchResultOwner::AcceptedInput {
            input: AcceptedInputId::from_uuid(owner),
            turn: TurnId::from_uuid(turn),
        }),
        ("transcript_entry", Some(turn)) => Ok(SearchResultOwner::TurnTranscriptEntry {
            entry: SemanticTranscriptEntryId::from_uuid(owner),
            turn: TurnId::from_uuid(turn),
        }),
        ("transcript_entry", None) => Ok(SearchResultOwner::SessionTranscriptEntry {
            entry: SemanticTranscriptEntryId::from_uuid(owner),
        }),
        ("tool_request", Some(turn)) => Ok(SearchResultOwner::ToolRequest {
            request: ToolRequestId::from_uuid(owner),
            turn: TurnId::from_uuid(turn),
        }),
        ("tool_attempt", Some(turn)) => Ok(SearchResultOwner::ToolAttempt {
            attempt: ToolAttemptId::from_uuid(owner),
            turn: TurnId::from_uuid(turn),
        }),
        ("attachment", None) => Ok(SearchResultOwner::Attachment {
            attachment: SearchArtifactId::from_uuid(owner),
        }),
        ("derived_artifact", None) => Ok(SearchResultOwner::DerivedArtifact {
            artifact: SearchArtifactId::from_uuid(owner),
        }),
        (
            "session" | "accepted_input" | "tool_request" | "tool_attempt" | "attachment"
            | "derived_artifact",
            _,
        ) => Err(SearchProjectionCorruption::OwnerShape),
        _ => Err(SearchProjectionCorruption::Unsupported {
            field: "owner kind",
            value: kind.to_owned(),
        }),
    }
}

fn decode_content_class(value: String) -> Result<SearchContentClass, SearchProjectionCorruption> {
    match value.as_str() {
        "user_transcript" => Ok(SearchContentClass::UserTranscript),
        "assistant_transcript" => Ok(SearchContentClass::AssistantTranscript),
        "tool_arguments" => Ok(SearchContentClass::ToolArguments),
        "tool_result" => Ok(SearchContentClass::ToolResult),
        "session_metadata" => Ok(SearchContentClass::SessionMetadata),
        "attachment_filename" => Ok(SearchContentClass::AttachmentFilename),
        "attachment_media_metadata" => Ok(SearchContentClass::AttachmentMediaMetadata),
        "derived_text_artifact" => Ok(SearchContentClass::DerivedTextArtifact),
        _ => Err(SearchProjectionCorruption::Unsupported {
            field: "content class",
            value,
        }),
    }
}

fn required_address(value: Decimal) -> Result<TimelineAddress, SearchProjectionCorruption> {
    if value.fract().is_zero()
        && !value.is_sign_negative()
        && let Ok(sequence) = u64::try_from(value)
        && let Some(sequence) = NonZeroU64::new(sequence)
    {
        return Ok(TimelineAddress::new(sequence));
    }
    Err(SearchProjectionCorruption::Invalid("timeline address"))
}

fn decode_headline(
    marked: String,
) -> Result<(String, Vec<SearchHighlight>), SearchProjectionCorruption> {
    let mut plain = String::new();
    let mut highlights = Vec::new();
    let mut active_start = None;
    let mut remaining = marked.as_str();
    while !remaining.is_empty() && plain.len() < max_search_snippet_bytes() {
        if let Some(rest) = remaining.strip_prefix(HEADLINE_START) {
            active_start = Some(plain.len());
            remaining = rest;
            continue;
        }
        if let Some(rest) = remaining.strip_prefix(HEADLINE_END) {
            append_highlight(&mut highlights, active_start.take(), plain.len())?;
            remaining = rest;
            continue;
        }
        let Some(character) = remaining.chars().next() else {
            break;
        };
        if plain.len() + character.len_utf8() > max_search_snippet_bytes() {
            break;
        }
        plain.push(character);
        remaining = &remaining[character.len_utf8()..];
    }
    append_highlight(&mut highlights, active_start, plain.len())?;
    Ok((plain, highlights))
}

fn append_highlight(
    highlights: &mut Vec<SearchHighlight>,
    start: Option<usize>,
    end: usize,
) -> Result<(), SearchProjectionCorruption> {
    let Some(start) = start.filter(|start| *start < end) else {
        return Ok(());
    };
    highlights.push(SearchHighlight {
        start_byte: u16::try_from(start)
            .map_err(|_| SearchProjectionCorruption::Invalid("highlight start"))?,
        end_byte: u16::try_from(end)
            .map_err(|_| SearchProjectionCorruption::Invalid("highlight end"))?,
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headline_decoder_returns_plain_bounded_text_and_byte_ranges() {
        const PREFIX: &str = "before <b>";
        const MATCH: &str = "café";
        const SUFFIX: &str = "</b> after";
        let marked = format!("{PREFIX}{HEADLINE_START}{MATCH}{HEADLINE_END}{SUFFIX}");
        let (snippet, highlights) = decode_headline(marked).expect("fixture headline decodes");

        assert_eq!(snippet, format!("{PREFIX}{MATCH}{SUFFIX}"));
        assert_eq!(
            highlights,
            vec![SearchHighlight {
                start_byte: u16::try_from(PREFIX.len()).expect("fixture offset fits"),
                end_byte: u16::try_from(PREFIX.len() + MATCH.len()).expect("fixture offset fits"),
            }]
        );
    }

    #[test]
    fn headline_decoder_caps_large_source_text() {
        let marked = format!(
            "{HEADLINE_START}{}{HEADLINE_END}",
            "x".repeat(max_search_snippet_bytes() * 2)
        );
        let (snippet, highlights) = decode_headline(marked).expect("fixture headline decodes");

        assert_eq!(snippet.len(), max_search_snippet_bytes());
        assert_eq!(
            highlights,
            vec![SearchHighlight {
                start_byte: 0,
                end_byte: u16::try_from(max_search_snippet_bytes())
                    .expect("snippet bound fits highlight offsets"),
            }]
        );
    }
}

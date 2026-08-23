//! PostgreSQL adapter for bounded application lexical search.

use std::{error::Error, fmt, num::NonZeroU64};

use rust_decimal::Decimal;
use signalbox_application::{
    SearchArtifactId, SearchArtifactProjection, SearchArtifactProjectionClass, SearchContentClass,
    SearchCursor, SearchHighlight, SearchPage, SearchProjectionWriter, SearchQuery, SearchReader,
    SearchResult, SearchResultSource, SearchScope, SearchStrategy, TimelineAddress,
    max_search_highlights_per_result, max_search_snippet_bytes,
};
use signalbox_domain::{
    AcceptedInputId, SemanticTranscriptEntryId, SessionId, ToolAttemptId, ToolRequestId, TurnId,
};
use sqlx::{PgPool, Row, postgres::PgRow};
use uuid::Uuid;

use crate::mapping::{
    SearchProjectionSourceKind, search_projection_content_class_from_str,
    search_projection_content_class_to_str, search_projection_source_kind_from_str,
    search_projection_source_kind_to_str,
};

#[cfg(test)]
const HEADLINE_START: &str = "\u{e000}";
#[cfg(test)]
const HEADLINE_END: &str = "\u{e001}";

const SEARCH_SQL: &str = "
WITH lexical_query AS (
    SELECT plainto_tsquery('simple'::regconfig, $1) AS value
), query_terms AS (
    SELECT DISTINCT unnest(
        tsvector_to_array(to_tsvector('simple'::regconfig, $1))
    ) AS lexeme
), candidate_sources AS MATERIALIZED (
    SELECT matching_chunk.source_kind, matching_chunk.source_id,
           matching_chunk.content_class
      FROM query_terms AS term
      JOIN web_search_projection AS matching_chunk
        ON matching_chunk.search_vector @@ to_tsquery(
               'simple'::regconfig, quote_literal(term.lexeme)
           )
       AND ($2::uuid IS NULL OR matching_chunk.session_id = $2)
     GROUP BY matching_chunk.source_kind, matching_chunk.source_id,
              matching_chunk.content_class
    HAVING count(DISTINCT term.lexeme) = (SELECT count(*) FROM query_terms)
)
SELECT projection.projection_id, projection.session_id,
       projection.event_sequence, projection.item_kind, projection.item_id,
       projection.source_kind, projection.source_id, turn_id, content_class,
       NOT EXISTS (
           SELECT 1
             FROM web_search_projection AS correlated_chunk
            WHERE correlated_chunk.source_kind = projection.source_kind
              AND correlated_chunk.source_id = projection.source_id
              AND correlated_chunk.content_class = projection.content_class
              AND (
                  correlated_chunk.session_id <> projection.session_id
                  OR correlated_chunk.event_sequence <> projection.event_sequence
                  OR correlated_chunk.item_kind <> projection.item_kind
                  OR correlated_chunk.item_id <> projection.item_id
                  OR correlated_chunk.turn_id IS DISTINCT FROM projection.turn_id
              )
       ) AS source_group_valid,
       '<sb-search-start>' AS start_marker,
       '<sb-search-stop>' AS stop_marker,
       ts_headline(
           'simple'::regconfig,
           replace(
               replace(projection.content_text, '&', '&amp;'),
               '<', '&lt;'
           ),
           lexical_query.value,
           'StartSel=<sb-search-start>, StopSel=<sb-search-stop>' ||
           ', MaxWords=32, MinWords=8, ShortWord=1, MaxFragments=1'
       ) AS marked_snippet
  FROM web_search_projection AS projection
  JOIN candidate_sources
    ON candidate_sources.source_kind = projection.source_kind
   AND candidate_sources.source_id = projection.source_id
   AND candidate_sources.content_class = projection.content_class
 CROSS JOIN lexical_query
 WHERE projection.projection_id = (
       SELECT min(candidate.projection_id)
         FROM web_search_projection AS candidate
        WHERE candidate.source_kind = projection.source_kind
          AND candidate.source_id = projection.source_id
          AND candidate.content_class = projection.content_class
          AND candidate.session_id = projection.session_id
          AND candidate.event_sequence = projection.event_sequence
          AND candidate.item_kind = projection.item_kind
          AND candidate.item_id = projection.item_id
          AND candidate.turn_id IS NOT DISTINCT FROM projection.turn_id
          AND EXISTS (
              SELECT 1
                FROM query_terms AS term
               WHERE candidate.search_vector @@ to_tsquery(
                   'simple'::regconfig, quote_literal(term.lexeme)
               )
          )
   )
   AND ($2::uuid IS NULL OR projection.session_id = $2)
   AND (
       $3::numeric IS NULL
       OR projection.event_sequence < $3
       OR (
           projection.event_sequence = $3
           AND projection.projection_id < $4
       )
   )
 ORDER BY projection.event_sequence DESC, projection.projection_id DESC
 LIMIT $5";

const PUBLISH_ARTIFACT_SQL: &str = "
WITH chunks AS MATERIALIZED (
    SELECT * FROM web_search_projection_chunks($6)
), existing AS MATERIALIZED (
    SELECT projection.*
      FROM web_search_projection AS projection
     WHERE projection.source_kind = $1
       AND projection.source_id = $2
       AND projection.content_class = $5
), compatible AS MATERIALIZED (
    SELECT NOT EXISTS (SELECT 1 FROM existing)
        OR (
            NOT EXISTS (
                SELECT 1
                  FROM existing
                 WHERE session_id <> $3
                    OR event_sequence <> $4
                    OR item_kind <> $1
                    OR item_id <> $2
                    OR turn_id IS NOT NULL
            )
            AND NOT EXISTS (
                SELECT 1
                  FROM existing
                 WHERE NOT EXISTS (
                     SELECT 1
                       FROM chunks
                      WHERE chunks.ordinal = existing.projection_ordinal
                        AND chunks.content_text = existing.content_text
                 )
            )
            AND NOT EXISTS (
                SELECT 1
                  FROM chunks
                 WHERE NOT EXISTS (
                     SELECT 1
                       FROM existing
                      WHERE existing.projection_ordinal = chunks.ordinal
                        AND existing.content_text = chunks.content_text
                 )
            )
        ) AS value
), published AS (
INSERT INTO web_search_projection (
    source_kind, source_id, session_id, event_sequence,
    item_kind, item_id, turn_id, content_class, projection_ordinal, content_text
) SELECT $1, $2, $3, $4, $1, $2, NULL, $5,
         chunks.ordinal, chunks.content_text
    FROM chunks
   CROSS JOIN compatible
   WHERE compatible.value
ON CONFLICT (
    source_kind, source_id, content_class, projection_ordinal
) DO NOTHING
RETURNING 1
)
SELECT value FROM compatible";

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
    /// Stored source fields contradicted the selected typed source.
    SourceShape,
}

impl fmt::Display for SearchProjectionCorruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(field) => write!(formatter, "invalid search projection {field}"),
            Self::Unsupported { field, value } => {
                write!(formatter, "unsupported search projection {field}: {value}")
            }
            Self::SourceShape => formatter.write_str("search projection source shape is invalid"),
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
            SearchArtifactProjectionClass::AttachmentFilename => (
                SearchProjectionSourceKind::Attachment,
                SearchContentClass::AttachmentFilename,
            ),
            SearchArtifactProjectionClass::AttachmentMediaMetadata => (
                SearchProjectionSourceKind::Attachment,
                SearchContentClass::AttachmentMediaMetadata,
            ),
            SearchArtifactProjectionClass::DerivedText => (
                SearchProjectionSourceKind::DerivedArtifact,
                SearchContentClass::DerivedTextArtifact,
            ),
        };
        let source_kind = search_projection_source_kind_to_str(source_kind);
        let content_class = search_projection_content_class_to_str(content_class);
        let mut transaction = self.pool.begin().await?;
        let address_belongs_to_session = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (
                 SELECT 1
                   FROM outbox_event
                  WHERE session_id = $1 AND event_sequence = $2
                 UNION ALL
                 SELECT 1
                   FROM delegation_outbox_event
                  WHERE session_id = $1 AND event_sequence = $2
             )",
        )
        .bind(projection.session.into_uuid())
        .bind(Decimal::from(projection.address.sequence().get()))
        .fetch_one(&mut *transaction)
        .await?;
        if !address_belongs_to_session {
            transaction.rollback().await?;
            return Err(SearchProjectionCorruption::Invalid("artifact timeline address").into());
        }
        sqlx::query(
            "SELECT pg_advisory_xact_lock(
                 hashtextextended(
                     concat_ws(chr(31), $1::text, $2::text),
                     0
                 )
             )",
        )
        .bind(source_kind)
        .bind(projection.artifact.into_uuid())
        .execute(&mut *transaction)
        .await?;
        let identity_compatible = sqlx::query_scalar::<_, bool>(
            "SELECT NOT EXISTS (
                 SELECT 1
                   FROM web_search_projection
                  WHERE source_kind = $1
                    AND source_id = $2
                    AND session_id <> $3
             )",
        )
        .bind(source_kind)
        .bind(projection.artifact.into_uuid())
        .bind(projection.session.into_uuid())
        .fetch_one(&mut *transaction)
        .await?;
        if !identity_compatible {
            transaction.rollback().await?;
            return Err(
                SearchProjectionCorruption::Invalid("conflicting artifact identity").into(),
            );
        }
        let compatible = sqlx::query_scalar::<_, bool>(PUBLISH_ARTIFACT_SQL)
            .bind(source_kind)
            .bind(projection.artifact.into_uuid())
            .bind(projection.session.into_uuid())
            .bind(Decimal::from(projection.address.sequence().get()))
            .bind(content_class)
            .bind(projection.text.as_str())
            .fetch_one(&mut *transaction)
            .await?;
        if !compatible {
            transaction.rollback().await?;
            return Err(
                SearchProjectionCorruption::Invalid("conflicting artifact publication").into(),
            );
        }
        transaction.commit().await?;
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
    if !row.try_get::<bool, _>("source_group_valid")? {
        return Err(SearchProjectionCorruption::SourceShape.into());
    }
    let projection_id: i64 = row.try_get("projection_id")?;
    let projection = u64::try_from(projection_id)
        .ok()
        .and_then(NonZeroU64::new)
        .ok_or(SearchProjectionCorruption::Invalid("projection identity"))?;
    let address = required_address(row.try_get("event_sequence")?)?;
    let session = SessionId::from_uuid(row.try_get("session_id")?);
    let item_kind: String = row.try_get("item_kind")?;
    let item_id: Uuid = row.try_get("item_id")?;
    let source_kind: String = row.try_get("source_kind")?;
    let source_id: Uuid = row.try_get("source_id")?;
    let turn_id: Option<Uuid> = row.try_get("turn_id")?;
    let content_class = decode_content_class(row.try_get("content_class")?)?;
    validate_source_correlation(
        &source_kind,
        source_id,
        &item_kind,
        item_id,
        turn_id,
        content_class,
    )?;
    let source = decode_source(&source_kind, &item_kind, item_id, turn_id, session)?;
    let start_marker: String = row.try_get("start_marker")?;
    let stop_marker: String = row.try_get("stop_marker")?;
    let (snippet, highlights) =
        decode_headline(row.try_get("marked_snippet")?, &start_marker, &stop_marker)?;
    Ok((
        SearchCursor::new(address, projection),
        SearchResult {
            session,
            address,
            projection,
            source,
            content_class,
            snippet,
            highlights,
        },
    ))
}

fn validate_source_correlation(
    source_kind: &str,
    source_id: Uuid,
    item_kind: &str,
    item_id: Uuid,
    turn_id: Option<Uuid>,
    content_class: SearchContentClass,
) -> Result<(), SearchProjectionCorruption> {
    let correlated = source_id == item_id
        && matches!(
            (source_kind, item_kind, turn_id, content_class),
            (
                "accepted_input",
                "accepted_input",
                Some(_),
                SearchContentClass::UserTranscript
            ) | (
                "steering_input",
                "accepted_input",
                Some(_),
                SearchContentClass::UserTranscript
            ) | (
                "semantic_entry",
                "transcript_entry",
                Some(_),
                SearchContentClass::AssistantTranscript
            ) | (
                "semantic_entry",
                "transcript_entry",
                None,
                SearchContentClass::DerivedTextArtifact
            ) | (
                "tool_request",
                "tool_request",
                Some(_),
                SearchContentClass::ToolArguments
            ) | (
                "tool_attempt",
                "tool_attempt",
                Some(_),
                SearchContentClass::ToolResult
            ) | (
                "session_metadata",
                "session",
                None,
                SearchContentClass::SessionMetadata
            ) | (
                "attachment",
                "attachment",
                None,
                SearchContentClass::AttachmentFilename
            ) | (
                "attachment",
                "attachment",
                None,
                SearchContentClass::AttachmentMediaMetadata
            ) | (
                "derived_artifact",
                "derived_artifact",
                None,
                SearchContentClass::DerivedTextArtifact
            )
        );
    if correlated {
        Ok(())
    } else {
        Err(SearchProjectionCorruption::SourceShape)
    }
}

fn decode_source(
    source_kind: &str,
    item_kind: &str,
    source: Uuid,
    turn: Option<Uuid>,
    session: SessionId,
) -> Result<SearchResultSource, SearchProjectionCorruption> {
    let source_kind = search_projection_source_kind_from_str(source_kind).ok_or_else(|| {
        SearchProjectionCorruption::Unsupported {
            field: "source kind",
            value: source_kind.to_owned(),
        }
    })?;
    match (source_kind, item_kind, turn) {
        (SearchProjectionSourceKind::SessionMetadata, "session", None)
            if source == session.into_uuid() =>
        {
            Ok(SearchResultSource::Session(session))
        }
        (SearchProjectionSourceKind::AcceptedInput, "accepted_input", Some(turn)) => {
            Ok(SearchResultSource::AcceptedInput {
                input: AcceptedInputId::from_uuid(source),
                turn: TurnId::from_uuid(turn),
            })
        }
        (SearchProjectionSourceKind::SteeringInput, "accepted_input", Some(source_turn)) => {
            Ok(SearchResultSource::SteeringInput {
                input: AcceptedInputId::from_uuid(source),
                source_turn: TurnId::from_uuid(source_turn),
            })
        }
        (SearchProjectionSourceKind::SemanticEntry, "transcript_entry", Some(turn)) => {
            Ok(SearchResultSource::TurnTranscriptEntry {
                entry: SemanticTranscriptEntryId::from_uuid(source),
                turn: TurnId::from_uuid(turn),
            })
        }
        (SearchProjectionSourceKind::SemanticEntry, "transcript_entry", None) => {
            Ok(SearchResultSource::SessionTranscriptEntry {
                entry: SemanticTranscriptEntryId::from_uuid(source),
            })
        }
        (SearchProjectionSourceKind::ToolRequest, "tool_request", Some(turn)) => {
            Ok(SearchResultSource::ToolRequest {
                request: ToolRequestId::from_uuid(source),
                turn: TurnId::from_uuid(turn),
            })
        }
        (SearchProjectionSourceKind::ToolAttempt, "tool_attempt", Some(turn)) => {
            Ok(SearchResultSource::ToolAttempt {
                attempt: ToolAttemptId::from_uuid(source),
                turn: TurnId::from_uuid(turn),
            })
        }
        (SearchProjectionSourceKind::Attachment, "attachment", None) => {
            Ok(SearchResultSource::Attachment {
                attachment: SearchArtifactId::from_uuid(source),
            })
        }
        (SearchProjectionSourceKind::DerivedArtifact, "derived_artifact", None) => {
            Ok(SearchResultSource::DerivedArtifact {
                artifact: SearchArtifactId::from_uuid(source),
            })
        }
        _ => Err(SearchProjectionCorruption::SourceShape),
    }
}

fn decode_content_class(value: String) -> Result<SearchContentClass, SearchProjectionCorruption> {
    search_projection_content_class_from_str(&value).ok_or(
        SearchProjectionCorruption::Unsupported {
            field: "content class",
            value,
        },
    )
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
    start_marker: &str,
    stop_marker: &str,
) -> Result<(String, Vec<SearchHighlight>), SearchProjectionCorruption> {
    let mut plain = String::new();
    let mut marked_ranges = Vec::new();
    let mut active_start = None;
    let mut remaining = marked.as_str();
    while !remaining.is_empty() {
        if let Some(rest) = remaining.strip_prefix(start_marker) {
            active_start = Some(plain.len());
            remaining = rest;
            continue;
        }
        if let Some(rest) = remaining.strip_prefix(stop_marker) {
            append_marked_range(&mut marked_ranges, active_start.take(), plain.len());
            remaining = rest;
            continue;
        }
        if let Some(rest) = remaining.strip_prefix("&lt;") {
            plain.push('<');
            remaining = rest;
            continue;
        }
        if let Some(rest) = remaining.strip_prefix("&amp;") {
            plain.push('&');
            remaining = rest;
            continue;
        }
        let Some(character) = remaining.chars().next() else {
            break;
        };
        plain.push(character);
        remaining = &remaining[character.len_utf8()..];
    }
    append_marked_range(&mut marked_ranges, active_start, plain.len());

    let (window_start, window_end) = snippet_window(&plain, marked_ranges.first().copied());
    let snippet = plain[window_start..window_end].to_owned();
    let clipped_ranges = marked_ranges
        .into_iter()
        .filter_map(|(start, end)| {
            let clipped_start = start.max(window_start);
            let clipped_end = end.min(window_end);
            (clipped_start < clipped_end).then_some((clipped_start, clipped_end))
        })
        .collect::<Vec<_>>();
    if clipped_ranges.len() > max_search_highlights_per_result() {
        return Err(SearchProjectionCorruption::Invalid("highlight count"));
    }
    let highlights = clipped_ranges
        .into_iter()
        .map(|(start, end)| {
            Ok(SearchHighlight {
                start_byte: u16::try_from(start - window_start)
                    .map_err(|_| SearchProjectionCorruption::Invalid("highlight start"))?,
                end_byte: u16::try_from(end - window_start)
                    .map_err(|_| SearchProjectionCorruption::Invalid("highlight end"))?,
            })
        })
        .collect::<Result<Vec<_>, SearchProjectionCorruption>>()?;
    Ok((snippet, highlights))
}

fn append_marked_range(ranges: &mut Vec<(usize, usize)>, start: Option<usize>, end: usize) {
    let Some(start) = start.filter(|start| *start < end) else {
        return;
    };
    ranges.push((start, end));
}

fn snippet_window(plain: &str, first_match: Option<(usize, usize)>) -> (usize, usize) {
    let bound = max_search_snippet_bytes();
    if plain.len() <= bound {
        return (0, plain.len());
    }
    let desired_start = first_match
        .map(|(start, end)| {
            let match_len = end - start;
            start.saturating_sub(bound.saturating_sub(match_len) / 2)
        })
        .unwrap_or(0)
        .min(plain.len() - bound);
    let mut start = desired_start;
    while !plain.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = (start + bound).min(plain.len());
    while !plain.is_char_boundary(end) {
        end -= 1;
    }
    (start, end)
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
        let (snippet, highlights) = decode_headline(marked, HEADLINE_START, HEADLINE_END)
            .expect("fixture headline decodes");

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
        let (snippet, highlights) = decode_headline(marked, HEADLINE_START, HEADLINE_END)
            .expect("fixture headline decodes");

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

    #[test]
    fn headline_decoder_keeps_a_match_after_a_long_unmatched_prefix() {
        let marked = format!(
            "{}{HEADLINE_START}needle{HEADLINE_END} after",
            "x".repeat(max_search_snippet_bytes() + 64)
        );
        let (snippet, highlights) = decode_headline(marked, HEADLINE_START, HEADLINE_END)
            .expect("fixture headline decodes");

        assert!(snippet.contains("needle"));
        assert_eq!(highlights.len(), 1);
        let highlight = highlights[0];
        assert_eq!(
            &snippet[usize::from(highlight.start_byte)..usize::from(highlight.end_byte)],
            "needle"
        );
        assert!(snippet.len() <= max_search_snippet_bytes());
    }

    #[test]
    fn headline_decoder_preserves_literal_private_use_characters() {
        const START: &str = "<search-start>";
        const STOP: &str = "<search-stop>";
        let marked = format!("literal {HEADLINE_START} {START}needle{STOP} {HEADLINE_END}");
        let (snippet, highlights) =
            decode_headline(marked, START, STOP).expect("fixture headline decodes");

        assert_eq!(
            snippet,
            format!("literal {HEADLINE_START} needle {HEADLINE_END}")
        );
        assert_eq!(highlights.len(), 1);
        let highlight = highlights[0];
        assert_eq!(
            &snippet[usize::from(highlight.start_byte)..usize::from(highlight.end_byte)],
            "needle"
        );
    }

    #[test]
    fn headline_decoder_restores_escaped_framing_text() {
        const START: &str = "<sb-search-start>";
        const STOP: &str = "<sb-search-stop>";
        let marked =
            format!("&lt;sb-search-start> &amp;lt; {START}needle{STOP} &lt;sb-search-stop>");
        let (snippet, highlights) =
            decode_headline(marked, START, STOP).expect("escaped framing text decodes");

        assert_eq!(snippet, "<sb-search-start> &lt; needle <sb-search-stop>");
        assert_eq!(highlights.len(), 1);
        let highlight = highlights[0];
        assert_eq!(
            &snippet[usize::from(highlight.start_byte)..usize::from(highlight.end_byte)],
            "needle"
        );
    }
}

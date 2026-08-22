//! Bounded lexical-search query vocabulary and orchestration.
//!
//! Search product semantics are application-owned. Persistence adapters may
//! implement the lexical strategy with PostgreSQL, but callers never provide
//! database query syntax or observe storage rows.

use std::{fmt, future::Future, num::NonZeroU64};

use signalbox_domain::{
    AcceptedInputId, SemanticTranscriptEntryId, SessionId, ToolAttemptId, ToolRequestId, TurnId,
};
use uuid::Uuid;

/// Maximum UTF-8 bytes accepted in one product search expression.
#[must_use]
pub const fn max_search_query_bytes() -> usize {
    512
}

/// Maximum records returned by one search page.
#[must_use]
pub const fn max_search_page_items() -> u16 {
    100
}

/// Maximum UTF-8 bytes retained in one result snippet.
#[must_use]
pub const fn max_search_snippet_bytes() -> usize {
    512
}

/// Maximum UTF-8 bytes accepted in one explicit artifact projection.
#[must_use]
pub const fn max_search_projection_text_bytes() -> usize {
    1_048_576
}

/// Rejection of product search text before it reaches a strategy adapter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SearchTextError {
    /// The expression was empty or only whitespace.
    Empty,
    /// The expression crossed the hard UTF-8 byte ceiling.
    TooLong,
    /// The expression contained NUL.
    ContainsNul,
}

impl fmt::Display for SearchTextError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Empty => "search text is empty",
            Self::TooLong => "search text exceeds its byte bound",
            Self::ContainsNul => "search text contains NUL",
        })
    }
}

impl std::error::Error for SearchTextError {}

/// One validated natural-language lexical expression.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchText(String);

impl SearchText {
    /// Admits nonempty, bounded, NUL-free text without interpreting operators.
    pub fn try_new(value: String) -> Result<Self, SearchTextError> {
        if value.trim().is_empty() {
            return Err(SearchTextError::Empty);
        }
        if value.len() > max_search_query_bytes() {
            return Err(SearchTextError::TooLong);
        }
        if value.contains('\0') {
            return Err(SearchTextError::ContainsNul);
        }
        Ok(Self(value))
    }

    /// Borrows the exact admitted expression.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Search strategy selected at the application boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SearchStrategy {
    /// PostgreSQL-backed lexical full-text matching in version one.
    Lexical,
}

/// Corpus selected for one bounded query.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SearchScope {
    /// Every indexed session.
    Global,
    /// One exact session.
    Session(SessionId),
}

/// Rejection of a requested search page size.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SearchPageLimitError;

impl fmt::Display for SearchPageLimitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("search page size is outside its hard bounds")
    }
}

impl std::error::Error for SearchPageLimitError {}

/// Validated item ceiling for one search page.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SearchPageLimit(u16);

impl SearchPageLimit {
    /// Admits one through the application hard ceiling.
    pub const fn new(value: u16) -> Result<Self, SearchPageLimitError> {
        if value == 0 || value > max_search_page_items() {
            Err(SearchPageLimitError)
        } else {
            Ok(Self(value))
        }
    }

    /// Returns the admitted page size.
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }
}

/// Stable descending keyset boundary for a search page.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SearchCursor {
    address: super::TimelineAddress,
    projection: NonZeroU64,
}

impl SearchCursor {
    /// Constructs an opaque logical cursor from durable ordering facts.
    #[must_use]
    pub const fn new(address: super::TimelineAddress, projection: NonZeroU64) -> Self {
        Self {
            address,
            projection,
        }
    }

    /// Returns the timeline component of the keyset boundary.
    #[must_use]
    pub const fn address(self) -> super::TimelineAddress {
        self.address
    }

    /// Returns the projection component of the keyset boundary.
    #[must_use]
    pub const fn projection(self) -> NonZeroU64 {
        self.projection
    }
}

/// Complete bounded lexical-search request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchQuery {
    /// Selected strategy boundary.
    pub strategy: SearchStrategy,
    /// Global or exact-session corpus.
    pub scope: SearchScope,
    /// Natural-language product expression.
    pub text: SearchText,
    /// Maximum returned items.
    pub limit: SearchPageLimit,
    /// Optional strict descending keyset boundary.
    pub after: Option<SearchCursor>,
}

/// Content projection whose text produced a lexical match.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SearchContentClass {
    /// Canonical accepted user transcript text.
    UserTranscript,
    /// Canonical final assistant transcript text.
    AssistantTranscript,
    /// Model-visible tool request arguments.
    ToolArguments,
    /// Model-visible successful tool result text.
    ToolResult,
    /// Current title, tags, and searchable metadata.
    SessionMetadata,
    /// Explicit attachment display filename.
    AttachmentFilename,
    /// Explicit attachment media metadata.
    AttachmentMediaMetadata,
    /// Durable text produced by an explicit derivation.
    DerivedTextArtifact,
}

/// Stable identity of an explicitly projected attachment or derived artifact.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SearchArtifactId(Uuid);

impl SearchArtifactId {
    /// Reconstitutes an application search-artifact identity.
    #[must_use]
    pub const fn from_uuid(value: Uuid) -> Self {
        Self(value)
    }

    /// Returns the UUID representation used at adapter boundaries.
    #[must_use]
    pub const fn into_uuid(self) -> Uuid {
        self.0
    }
}

/// Rejection of text supplied by an explicit durable artifact publisher.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SearchProjectionTextError {
    /// The projection was empty.
    Empty,
    /// The projection crossed the hard UTF-8 byte ceiling.
    TooLong,
    /// The projection contained NUL.
    ContainsNul,
}

impl fmt::Display for SearchProjectionTextError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Empty => "search projection text is empty",
            Self::TooLong => "search projection text exceeds its byte bound",
            Self::ContainsNul => "search projection text contains NUL",
        })
    }
}

impl std::error::Error for SearchProjectionTextError {}

/// Validated text owned by an explicit attachment or derivation producer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchProjectionText(String);

impl SearchProjectionText {
    /// Admits nonempty, bounded, NUL-free projection text.
    pub fn try_new(value: String) -> Result<Self, SearchProjectionTextError> {
        if value.is_empty() {
            return Err(SearchProjectionTextError::Empty);
        }
        if value.len() > max_search_projection_text_bytes() {
            return Err(SearchProjectionTextError::TooLong);
        }
        if value.contains('\0') {
            return Err(SearchProjectionTextError::ContainsNul);
        }
        Ok(Self(value))
    }

    /// Borrows the exact admitted projection.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Explicit projection class published only after its durable source exists.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SearchArtifactProjectionClass {
    /// Attachment display filename.
    AttachmentFilename,
    /// Attachment media metadata supplied by its owning contract.
    AttachmentMediaMetadata,
    /// Text produced by an explicit durable derivation.
    DerivedText,
}

/// One artifact-owned projection anchored to a stable session event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchArtifactProjection {
    /// Session containing the owning artifact.
    pub session: SessionId,
    /// Stable event address at which the artifact became visible.
    pub address: super::TimelineAddress,
    /// Stable identity supplied by the owning artifact contract.
    pub artifact: SearchArtifactId,
    /// Projection/content class.
    pub class: SearchArtifactProjectionClass,
    /// Explicit text; no extractor, OCR, or model pass is implied.
    pub text: SearchProjectionText,
}

/// Write boundary for artifact producers that already committed durable text.
pub trait SearchProjectionWriter {
    /// Adapter-specific infrastructure or integrity failure.
    type Error;

    /// Publishes one idempotent explicit projection.
    fn publish(
        &self,
        projection: SearchArtifactProjection,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// Typed durable item that owns one matched projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SearchResultSource {
    /// Session-level metadata, anchored to the session timeline.
    Session(SessionId),
    /// Canonical user input and its origin turn.
    AcceptedInput {
        /// Exact accepted input identity.
        input: AcceptedInputId,
        /// Exact origin turn identity.
        turn: TurnId,
    },
    /// Consumed next-safe-point steering input and the turn that supplied it.
    SteeringInput {
        /// Exact accepted input identity.
        input: AcceptedInputId,
        /// Exact turn from which the steering input was supplied.
        source_turn: TurnId,
    },
    /// Semantic transcript entry owned by one turn.
    TurnTranscriptEntry {
        /// Exact semantic entry identity.
        entry: SemanticTranscriptEntryId,
        /// Exact owning turn identity.
        turn: TurnId,
    },
    /// Session-owned semantic entry, such as a derived context summary.
    SessionTranscriptEntry {
        /// Exact semantic entry identity.
        entry: SemanticTranscriptEntryId,
    },
    /// Tool request and its owning turn.
    ToolRequest {
        /// Exact request identity.
        request: ToolRequestId,
        /// Exact owning turn identity.
        turn: TurnId,
    },
    /// Tool attempt and its owning turn.
    ToolAttempt {
        /// Exact attempt identity.
        attempt: ToolAttemptId,
        /// Exact owning turn identity.
        turn: TurnId,
    },
    /// Explicit attachment metadata projection.
    Attachment {
        /// Stable attachment projection identity.
        attachment: SearchArtifactId,
    },
    /// Explicit durable derived-text projection.
    DerivedArtifact {
        /// Stable derived-artifact projection identity.
        artifact: SearchArtifactId,
    },
}

/// One highlighted half-open UTF-8 byte range within a result snippet.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SearchHighlight {
    /// Inclusive UTF-8 byte offset.
    pub start_byte: u16,
    /// Exclusive UTF-8 byte offset.
    pub end_byte: u16,
}

/// One bounded lexical match with a stable history reveal address.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchResult {
    /// Session containing the matched projection.
    pub session: SessionId,
    /// Stable address used by an `around` timeline read.
    pub address: super::TimelineAddress,
    /// Typed durable source rather than a storage record discriminator.
    pub source: SearchResultSource,
    /// Projection whose text matched.
    pub content_class: SearchContentClass,
    /// Bounded plain-text context.
    pub snippet: String,
    /// Bounded ranges into `snippet`.
    pub highlights: Vec<SearchHighlight>,
}

/// One bounded page and its strict continuation, if another match exists.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchPage {
    /// Results in stable newest-address-first order.
    pub results: Vec<SearchResult>,
    /// Cursor for the next strict page.
    pub next: Option<SearchCursor>,
}

/// Application-owned strategy boundary for bounded search.
pub trait SearchReader {
    /// Adapter-specific infrastructure or integrity failure.
    type Error;

    /// Executes one validated query without materializing the corpus.
    fn search(
        &self,
        query: SearchQuery,
    ) -> impl Future<Output = Result<SearchPage, Self::Error>> + Send;
}

/// Coordinates product search without exposing adapter query syntax.
#[derive(Debug)]
pub struct SearchService<Reader> {
    reader: Reader,
}

impl<Reader> SearchService<Reader> {
    /// Wraps one search adapter.
    #[must_use]
    pub const fn new(reader: Reader) -> Self {
        Self { reader }
    }
}

impl<Reader: SearchReader> SearchService<Reader> {
    /// Executes a bounded search through the selected strategy.
    pub async fn search(&self, query: SearchQuery) -> Result<SearchPage, Reader::Error> {
        self.reader.search(query).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_text_rejects_empty_nul_and_over_bound_expressions() {
        assert_eq!(
            SearchText::try_new(String::from("  \n")),
            Err(SearchTextError::Empty)
        );
        assert_eq!(
            SearchText::try_new(String::from("a\0b")),
            Err(SearchTextError::ContainsNul)
        );
        assert_eq!(
            SearchText::try_new("x".repeat(max_search_query_bytes() + 1)),
            Err(SearchTextError::TooLong)
        );
    }

    #[test]
    fn search_page_limit_rejects_zero_and_values_above_the_hard_ceiling() {
        assert_eq!(SearchPageLimit::new(0), Err(SearchPageLimitError));
        assert_eq!(
            SearchPageLimit::new(max_search_page_items() + 1),
            Err(SearchPageLimitError)
        );
    }

    #[test]
    fn explicit_projection_text_rejects_empty_nul_and_over_bound_values() {
        assert_eq!(
            SearchProjectionText::try_new(String::new()),
            Err(SearchProjectionTextError::Empty)
        );
        assert_eq!(
            SearchProjectionText::try_new(String::from("a\0b")),
            Err(SearchProjectionTextError::ContainsNul)
        );
        assert_eq!(
            SearchProjectionText::try_new("x".repeat(max_search_projection_text_bytes() + 1)),
            Err(SearchProjectionTextError::TooLong)
        );
    }
}

//! Unified conversation listing across native sessions and imported
//! conversations.
//!
//! The listing is one bounded keyset read over the authoritative session and
//! imported-conversation tables; it unifies the view, never the storage
//! (docs/spec/process-protocol.md).

use std::future::Future;

use signalbox_domain::{
    ImportedConversationFormat, ImportedConversationId, SessionConfigurationDefaultsVersion,
    SessionId, SessionMetadataContent,
};

/// Which conversation origin classes one unified list query selects.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConversationOriginFilter {
    /// Native sessions only.
    Native,
    /// Imported conversations only.
    Imported,
    /// Both origin classes.
    All,
}

impl ConversationOriginFilter {
    /// Returns whether native sessions participate.
    pub const fn selects_native(self) -> bool {
        matches!(self, Self::Native | Self::All)
    }

    /// Returns whether imported conversations participate.
    pub const fn selects_imported(self) -> bool {
        matches!(self, Self::Imported | Self::All)
    }
}

/// One exclusive unified keyset cursor naming the last listed conversation.
///
/// The unified order is by conversation identity UUID value, with a native
/// session ordered before an imported conversation carrying the same
/// identity value, so one cursor names one total position across both origin
/// classes and no row is silently skipped at a page boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConversationListCursor {
    /// The named position is a native session.
    NativeSession(SessionId),
    /// The named position is an imported conversation.
    ImportedConversation(ImportedConversationId),
}

impl ConversationListCursor {
    /// Returns the cursor position's identity as its raw UUID value.
    pub const fn identity_uuid(self) -> uuid::Uuid {
        match self {
            Self::NativeSession(session) => session.into_uuid(),
            Self::ImportedConversation(conversation) => conversation.into_uuid(),
        }
    }
}

/// One validated unified conversation list query.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConversationListQuery {
    title_contains: Option<String>,
    origin: ConversationOriginFilter,
    include_archived: bool,
    page_size: u64,
    after: Option<ConversationListCursor>,
}

impl ConversationListQuery {
    /// Constructs the ordinary first page of the default unified view.
    pub fn default_page(page_size: u64) -> Self {
        Self {
            title_contains: None,
            origin: ConversationOriginFilter::All,
            include_archived: false,
            page_size,
            after: None,
        }
    }

    /// Validates the exact title filter and the inclusive page-size bound.
    pub fn try_new(
        title_contains: Option<String>,
        origin: ConversationOriginFilter,
        include_archived: bool,
        page_size: u64,
        after: Option<ConversationListCursor>,
    ) -> Result<Self, ConversationListQueryError> {
        Self::try_new_with_page_limits(
            title_contains,
            origin,
            include_archived,
            page_size,
            after,
            None,
            None,
        )
    }

    /// Validates the exact title filter and deployment page-size policies.
    #[allow(clippy::too_many_arguments)]
    pub fn try_new_with_page_limits(
        title_contains: Option<String>,
        origin: ConversationOriginFilter,
        include_archived: bool,
        page_size: u64,
        after: Option<ConversationListCursor>,
        min_page_size: Option<u64>,
        max_page_size: Option<u64>,
    ) -> Result<Self, ConversationListQueryError> {
        if let Some(query) = title_contains.as_deref() {
            if query.is_empty() {
                return Err(ConversationListQueryError::EmptyTitleSearch);
            }
            if query.contains('\0') {
                return Err(ConversationListQueryError::TitleSearchContainsNul);
            }
            if query.len() > SessionMetadataContent::MAX_TOTAL_UTF8_BYTES {
                return Err(ConversationListQueryError::TitleSearchExceedsUtf8Bytes);
            }
        }
        if min_page_size.is_some_and(|minimum| page_size < minimum)
            || max_page_size.is_some_and(|maximum| page_size > maximum)
        {
            return Err(ConversationListQueryError::PageSizeOutOfRange);
        }
        Ok(Self {
            title_contains,
            origin,
            include_archived,
            page_size,
            after,
        })
    }

    /// Borrows the exact optional case-sensitive title substring.
    pub fn title_contains(&self) -> Option<&str> {
        self.title_contains.as_deref()
    }

    /// Returns the selected origin classes.
    pub const fn origin(&self) -> ConversationOriginFilter {
        self.origin
    }

    /// Returns whether archived native sessions participate.
    pub const fn include_archived(&self) -> bool {
        self.include_archived
    }

    /// Returns the inclusive bounded maximum result count.
    pub const fn page_size(&self) -> u64 {
        self.page_size
    }

    /// Returns the exclusive unified keyset cursor.
    pub const fn after(&self) -> Option<ConversationListCursor> {
        self.after
    }
}

/// Why a unified conversation list query could not be constructed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConversationListQueryError {
    /// A present title substring was empty.
    EmptyTitleSearch,
    /// A present title substring contained U+0000.
    TitleSearchContainsNul,
    /// A present title substring exceeded the query UTF-8 byte bound.
    TitleSearchExceedsUtf8Bytes,
    /// Page size was outside 1 through 100.
    PageSizeOutOfRange,
}

/// One unified conversation list item.
///
/// A native row carries current organizational facts; an imported row carries
/// its immutable snapshot facts, including the total normalized entry count —
/// the greatest position a continuation may select. Neither row materializes
/// transcript or entry content.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConversationListItem {
    /// One native session.
    NativeSession {
        /// Listed session.
        session: SessionId,
        /// Exact optional metadata title.
        title: Option<String>,
        /// Whether the session is archived.
        archived: bool,
        /// Current defaults version.
        defaults_version: SessionConfigurationDefaultsVersion,
    },
    /// One immutable imported conversation snapshot.
    ImportedConversation {
        /// Listed imported conversation.
        conversation: ImportedConversationId,
        /// Exact optional source-derived display title.
        title: Option<String>,
        /// Total normalized entry count.
        entry_count: u64,
        /// Exact stored source format and converter version.
        format: ImportedConversationFormat,
    },
}

impl ConversationListItem {
    /// Returns the unified cursor position this item occupies.
    pub const fn cursor(&self) -> ConversationListCursor {
        match self {
            Self::NativeSession { session, .. } => ConversationListCursor::NativeSession(*session),
            Self::ImportedConversation { conversation, .. } => {
                ConversationListCursor::ImportedConversation(*conversation)
            }
        }
    }

    /// Borrows the exact optional display title.
    pub fn title(&self) -> Option<&str> {
        match self {
            Self::NativeSession { title, .. } | Self::ImportedConversation { title, .. } => {
                title.as_deref()
            }
        }
    }
}

/// One opened, repeatable-read unified conversation page.
pub trait ConversationPageReader {
    /// Adapter-specific infrastructure or integrity failure.
    type Error;

    /// Yields one item at a time in strict unified cursor order.
    fn next_item(
        &mut self,
    ) -> impl Future<Output = Result<Option<ConversationListItem>, Self::Error>> + Send;

    /// Returns the continuation cursor after [`Self::next_item`] yielded
    /// `None`. A caller must exhaust the page before reading this value.
    fn next_after(&self) -> Option<ConversationListCursor>;
}

/// Application-owned port for opening one unified conversation page.
pub trait ConversationLister {
    /// Adapter-specific infrastructure or integrity failure.
    type Error;
    /// The bounded page reader returned for a successful open.
    type Page: ConversationPageReader<Error = Self::Error>;

    /// Opens one repeatable-read page for the exact validated query.
    fn open_conversation_page(
        &self,
        query: ConversationListQuery,
    ) -> impl Future<Output = Result<Self::Page, Self::Error>> + Send;
}

/// Coordinates one unified conversation list-page query.
#[derive(Debug)]
pub struct ListConversationsService<Lister> {
    lister: Lister,
}

impl<Lister> ListConversationsService<Lister> {
    /// Composes the use case with its page-opening port.
    pub const fn new(lister: Lister) -> Self {
        Self { lister }
    }

    /// Returns the lister, primarily for explicit ownership handoff.
    pub fn into_lister(self) -> Lister {
        self.lister
    }
}

impl<Lister> ListConversationsService<Lister>
where
    Lister: ConversationLister,
{
    /// Opens exactly one page without retry or filter rewriting.
    pub async fn execute(
        &self,
        query: ConversationListQuery,
    ) -> Result<Lister::Page, Lister::Error> {
        self.lister.open_conversation_page(query).await
    }
}

#[cfg(test)]
mod tests {
    use std::future::{Future, ready};

    use signalbox_domain::SessionMetadataContent;
    use uuid::Uuid;

    use super::{
        ConversationListCursor, ConversationListItem, ConversationListQuery,
        ConversationListQueryError, ConversationLister, ConversationOriginFilter,
        ConversationPageReader, ImportedConversationId, ListConversationsService, SessionId,
    };

    fn session_id(value: u128) -> SessionId {
        SessionId::from_uuid(Uuid::from_u128(value))
    }

    fn imported_id(value: u128) -> ImportedConversationId {
        ImportedConversationId::from_uuid(Uuid::from_u128(value))
    }

    #[test]
    fn query_rejects_an_empty_title_search() {
        assert_eq!(
            ConversationListQuery::try_new(
                Some(String::new()),
                ConversationOriginFilter::All,
                false,
                50,
                None,
            ),
            Err(ConversationListQueryError::EmptyTitleSearch)
        );
    }

    #[test]
    fn query_rejects_a_title_search_carrying_nul() {
        assert_eq!(
            ConversationListQuery::try_new(
                Some(String::from("a\0b")),
                ConversationOriginFilter::All,
                false,
                50,
                None,
            ),
            Err(ConversationListQueryError::TitleSearchContainsNul)
        );
    }

    #[test]
    fn query_rejects_a_title_search_beyond_the_byte_bound() {
        assert_eq!(
            ConversationListQuery::try_new(
                Some("q".repeat(SessionMetadataContent::MAX_TOTAL_UTF8_BYTES + 1)),
                ConversationOriginFilter::All,
                false,
                50,
                None,
            ),
            Err(ConversationListQueryError::TitleSearchExceedsUtf8Bytes)
        );
    }

    #[test]
    fn query_rejects_page_sizes_outside_the_inclusive_bound() {
        assert_eq!(
            ConversationListQuery::try_new_with_page_limits(
                None,
                ConversationOriginFilter::All,
                false,
                1,
                None,
                Some(2),
                Some(7),
            ),
            Err(ConversationListQueryError::PageSizeOutOfRange)
        );
        assert_eq!(
            ConversationListQuery::try_new_with_page_limits(
                None,
                ConversationOriginFilter::All,
                false,
                8,
                None,
                Some(2),
                Some(7),
            ),
            Err(ConversationListQueryError::PageSizeOutOfRange)
        );
    }

    #[test]
    fn query_retains_every_admitted_field_exactly() {
        let after = ConversationListCursor::ImportedConversation(imported_id(7));
        let query = ConversationListQuery::try_new(
            Some(String::from("Active")),
            ConversationOriginFilter::Imported,
            true,
            25,
            Some(after),
        )
        .expect("admitted query is valid");

        assert_eq!(query.title_contains(), Some("Active"));
        assert_eq!(query.origin(), ConversationOriginFilter::Imported);
        assert!(query.include_archived());
        assert_eq!(query.page_size(), 25);
        assert_eq!(query.after(), Some(after));
    }

    #[test]
    fn default_page_selects_the_unfiltered_non_archived_unified_view() {
        let query = ConversationListQuery::default_page(5);

        assert_eq!(query.title_contains(), None);
        assert_eq!(query.origin(), ConversationOriginFilter::All);
        assert!(!query.include_archived());
        assert_eq!(query.page_size(), 5);
        assert_eq!(query.after(), None);
    }

    #[test]
    fn origin_filter_selects_exactly_its_named_classes() {
        assert!(ConversationOriginFilter::Native.selects_native());
        assert!(!ConversationOriginFilter::Native.selects_imported());
        assert!(!ConversationOriginFilter::Imported.selects_native());
        assert!(ConversationOriginFilter::Imported.selects_imported());
        assert!(ConversationOriginFilter::All.selects_native());
        assert!(ConversationOriginFilter::All.selects_imported());
    }

    #[test]
    fn item_cursor_names_the_item_position() {
        let native = ConversationListItem::NativeSession {
            session: session_id(3),
            title: None,
            archived: false,
            defaults_version: signalbox_domain::SessionConfigurationDefaultsVersion::first(),
        };
        let imported = ConversationListItem::ImportedConversation {
            conversation: imported_id(4),
            title: Some(String::from("Imported title")),
            entry_count: 2,
            format: signalbox_domain::ImportedConversationFormat::CodexRolloutJsonlV1,
        };

        assert_eq!(
            native.cursor(),
            ConversationListCursor::NativeSession(session_id(3))
        );
        assert_eq!(
            imported.cursor(),
            ConversationListCursor::ImportedConversation(imported_id(4))
        );
        assert_eq!(native.title(), None);
        assert_eq!(imported.title(), Some("Imported title"));
    }

    #[derive(Debug)]
    struct FakePage;

    impl ConversationPageReader for FakePage {
        type Error = FakeError;

        fn next_item(
            &mut self,
        ) -> impl Future<Output = Result<Option<ConversationListItem>, Self::Error>> + Send
        {
            ready(Ok(None))
        }

        fn next_after(&self) -> Option<ConversationListCursor> {
            None
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum FakeError {
        Unavailable,
    }

    #[derive(Debug)]
    struct FakeLister {
        response: Result<(), FakeError>,
        observed: std::sync::Mutex<Vec<ConversationListQuery>>,
    }

    impl ConversationLister for FakeLister {
        type Error = FakeError;
        type Page = FakePage;

        fn open_conversation_page(
            &self,
            query: ConversationListQuery,
        ) -> impl Future<Output = Result<Self::Page, Self::Error>> + Send {
            self.observed
                .lock()
                .expect("fake lister lock is never poisoned")
                .push(query);
            ready(self.response.map(|()| FakePage))
        }
    }

    /// The service opens exactly one page for the exact validated query.
    #[tokio::test]
    async fn service_opens_one_page_for_the_exact_query() {
        let service = ListConversationsService::new(FakeLister {
            response: Ok(()),
            observed: std::sync::Mutex::new(Vec::new()),
        });
        let query = ConversationListQuery::default_page(5);

        service
            .execute(query.clone())
            .await
            .expect("fake lister opens the page");

        let lister = service.into_lister();
        let observed = lister
            .observed
            .into_inner()
            .expect("fake lister lock is never poisoned");
        assert_eq!(observed, vec![query]);
    }

    /// A lister failure surfaces unchanged without retry.
    #[tokio::test]
    async fn service_surfaces_the_lister_failure_without_retry() {
        let service = ListConversationsService::new(FakeLister {
            response: Err(FakeError::Unavailable),
            observed: std::sync::Mutex::new(Vec::new()),
        });

        let error = service
            .execute(ConversationListQuery::default_page(5))
            .await
            .expect_err("fake lister fails");

        assert_eq!(error, FakeError::Unavailable);
        let lister = service.into_lister();
        assert_eq!(
            lister
                .observed
                .into_inner()
                .expect("fake lister lock is never poisoned")
                .len(),
            1
        );
    }
}

//! Session-metadata read, list, and durable replacement orchestration.
//!
//! `docs/spec/sessions-and-transcript.md` owns the satellite snapshot, exact
//! filtering, keyset pagination, and last-writer behavior. This module owns
//! use-case requests and ports while keeping PostgreSQL records and
//! process-protocol frames outside the application boundary.

use std::{collections::BTreeSet, future::Future};

use signalbox_domain::{
    Actor, DangerousToolAutoApproval, DurableCommandId, ModelSelectionRequest,
    ReplaceSessionMetadata as DomainReplaceSessionMetadata, ReplaceSessionMetadataResult,
    SessionConfigurationDefaultsVersion, SessionId, SessionMetadataContent,
    SessionMetadataLastWriter, SessionMetadataSnapshot, ToolRequestId,
};

use crate::InvalidDurableCommandId;

/// The complete validated application request for metadata replacement.
///
/// Construction admits either the user boundary or execution of one exact
/// tool request; callers cannot supply an arbitrary actor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplaceSessionMetadataRequest {
    command_id: DurableCommandId,
    session: SessionId,
    issuer: SessionMetadataReplacementIssuer,
    replacement: SessionMetadataContent,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionMetadataReplacementIssuer {
    User,
    Tool(ToolRequestId),
}

impl ReplaceSessionMetadataRequest {
    /// Validates the command identity before canonical command construction.
    pub fn try_new(
        command_id: DurableCommandId,
        session: SessionId,
        replacement: SessionMetadataContent,
    ) -> Result<Self, InvalidDurableCommandId> {
        if command_id.as_uuid().is_nil() {
            return Err(InvalidDurableCommandId::Nil);
        }
        if command_id.as_uuid().is_max() {
            return Err(InvalidDurableCommandId::Max);
        }

        Ok(Self {
            command_id,
            session,
            issuer: SessionMetadataReplacementIssuer::User,
            replacement,
        })
    }

    /// Validates a tool-attributed command identity before canonical command
    /// construction.
    pub fn try_new_for_tool(
        command_id: DurableCommandId,
        session: SessionId,
        request: ToolRequestId,
        replacement: SessionMetadataContent,
    ) -> Result<Self, InvalidDurableCommandId> {
        if command_id.as_uuid().is_nil() {
            return Err(InvalidDurableCommandId::Nil);
        }
        if command_id.as_uuid().is_max() {
            return Err(InvalidDurableCommandId::Max);
        }

        Ok(Self {
            command_id,
            session,
            issuer: SessionMetadataReplacementIssuer::Tool(request),
            replacement,
        })
    }

    /// Returns the user-global durable command identity.
    pub const fn command_id(&self) -> DurableCommandId {
        self.command_id
    }

    /// Returns the target session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the exact initiating agency.
    pub const fn actor(&self) -> Actor {
        match self.issuer {
            SessionMetadataReplacementIssuer::User => Actor::User,
            SessionMetadataReplacementIssuer::Tool(request) => Actor::Tool { request },
        }
    }

    /// Borrows the complete replacement metadata.
    pub const fn replacement(&self) -> &SessionMetadataContent {
        &self.replacement
    }
}

/// The closed application result of one atomic metadata replacement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReplaceSessionMetadataOutcome {
    /// First handling or equal replay returned the recorded domain result.
    Recorded(ReplaceSessionMetadataResult),
    /// The user-global command identity already names another typed payload.
    ConflictingReuse {
        /// The identity whose existing meaning remains intact.
        command_id: DurableCommandId,
    },
}

/// Atomic command-handling boundary for complete metadata replacement.
pub trait ReplaceSessionMetadataTransaction {
    /// Adapter-specific infrastructure or integrity failure.
    type Error;

    /// Handles one canonical command through the durable claim boundary.
    fn handle(
        &mut self,
        command: DomainReplaceSessionMetadata,
    ) -> impl Future<Output = Result<ReplaceSessionMetadataOutcome, Self::Error>> + Send;
}

/// Coordinates complete session-metadata replacement.
#[derive(Debug)]
pub struct ReplaceSessionMetadataService<Transaction> {
    transaction: Transaction,
}

impl<Transaction> ReplaceSessionMetadataService<Transaction> {
    /// Composes the use case with its atomic transaction port.
    pub const fn new(transaction: Transaction) -> Self {
        Self { transaction }
    }

    /// Returns the transaction port, primarily for explicit ownership handoff.
    pub fn into_transaction(self) -> Transaction {
        self.transaction
    }
}

impl<Transaction> ReplaceSessionMetadataService<Transaction>
where
    Transaction: ReplaceSessionMetadataTransaction,
{
    /// Constructs one exactly attributed command and delegates exactly once.
    pub async fn execute(
        &mut self,
        request: ReplaceSessionMetadataRequest,
    ) -> Result<ReplaceSessionMetadataOutcome, Transaction::Error> {
        let command = match request.issuer {
            SessionMetadataReplacementIssuer::User => DomainReplaceSessionMetadata::new(
                request.command_id,
                request.session,
                request.replacement,
            ),
            SessionMetadataReplacementIssuer::Tool(tool_request) => {
                DomainReplaceSessionMetadata::new_for_tool(
                    request.command_id,
                    request.session,
                    tool_request,
                    request.replacement,
                )
            }
        };
        self.transaction.handle(command).await
    }
}

/// Application-owned port for loading one current metadata snapshot.
pub trait SessionMetadataReader {
    /// Adapter-specific infrastructure or integrity failure.
    type Error;

    /// Loads the complete snapshot selected by semantic session identity.
    ///
    /// `Ok(None)` means the session itself does not exist. A session with no
    /// metadata row returns `Some(SessionMetadataSnapshot::initial(...))`.
    fn load_session_metadata(
        &self,
        session: SessionId,
    ) -> impl Future<Output = Result<Option<SessionMetadataSnapshot>, Self::Error>> + Send;
}

/// Coordinates the single-session metadata read use case.
#[derive(Debug)]
pub struct LoadSessionMetadataService<Reader> {
    reader: Reader,
}

impl<Reader> LoadSessionMetadataService<Reader> {
    /// Composes the use case with its current metadata reader.
    pub const fn new(reader: Reader) -> Self {
        Self { reader }
    }

    /// Returns the reader, primarily for explicit ownership handoff.
    pub fn into_reader(self) -> Reader {
        self.reader
    }
}

impl<Reader> LoadSessionMetadataService<Reader>
where
    Reader: SessionMetadataReader,
{
    /// Loads one complete current metadata snapshot by semantic identity.
    pub async fn execute(
        &self,
        session: SessionId,
    ) -> Result<Option<SessionMetadataSnapshot>, Reader::Error> {
        self.reader.load_session_metadata(session).await
    }
}

/// One validated metadata list query.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionMetadataListQuery {
    required_tags: BTreeSet<String>,
    title_contains: Option<String>,
    include_archived: bool,
    page_size: u64,
    after_session: Option<SessionId>,
}

impl SessionMetadataListQuery {
    /// Constructs the ordinary first page of the default interactive view.
    pub fn default_page(page_size: u64) -> Self {
        Self {
            required_tags: BTreeSet::new(),
            title_contains: None,
            include_archived: false,
            page_size,
            after_session: None,
        }
    }

    /// Validates exact filters and the inclusive page-size bound.
    pub fn try_new(
        required_tags: Vec<String>,
        title_contains: Option<String>,
        include_archived: bool,
        page_size: u64,
        after_session: Option<SessionId>,
    ) -> Result<Self, SessionMetadataListQueryError> {
        Self::try_new_with_required_tag_limit(
            required_tags,
            title_contains,
            include_archived,
            page_size,
            after_session,
            None,
        )
    }

    /// Validates exact filters and the deployment's optional count policies.
    pub fn try_new_with_required_tag_limit(
        required_tags: Vec<String>,
        title_contains: Option<String>,
        include_archived: bool,
        page_size: u64,
        after_session: Option<SessionId>,
        max_required_tags: Option<usize>,
    ) -> Result<Self, SessionMetadataListQueryError> {
        Self::try_new_with_limits(
            required_tags,
            title_contains,
            include_archived,
            page_size,
            after_session,
            max_required_tags,
            None,
            None,
        )
    }

    /// Validates exact filters and all deployment list policies.
    #[allow(clippy::too_many_arguments)]
    pub fn try_new_with_limits(
        required_tags: Vec<String>,
        title_contains: Option<String>,
        include_archived: bool,
        page_size: u64,
        after_session: Option<SessionId>,
        max_required_tags: Option<usize>,
        min_page_size: Option<u64>,
        max_page_size: Option<u64>,
    ) -> Result<Self, SessionMetadataListQueryError> {
        if max_required_tags.is_some_and(|limit| required_tags.len() > limit) {
            return Err(SessionMetadataListQueryError::TooManyRequiredTags);
        }

        let mut total_utf8_bytes = 0usize;
        let mut canonical_tags = BTreeSet::new();
        for tag in required_tags {
            if tag.is_empty() {
                return Err(SessionMetadataListQueryError::EmptyTag);
            }
            if tag.contains('\0') {
                return Err(SessionMetadataListQueryError::TagContainsNul);
            }
            if tag.len() > SessionMetadataContent::MAX_INDEXED_UTF8_BYTES {
                return Err(SessionMetadataListQueryError::TagExceedsIndexedUtf8Bytes);
            }
            total_utf8_bytes = total_utf8_bytes.saturating_add(tag.len());
            if total_utf8_bytes > SessionMetadataContent::MAX_TOTAL_UTF8_BYTES {
                return Err(SessionMetadataListQueryError::TotalUtf8BytesExceeded);
            }
            if !canonical_tags.insert(tag) {
                return Err(SessionMetadataListQueryError::DuplicateTag);
            }
        }
        if let Some(query) = title_contains.as_deref() {
            if query.is_empty() {
                return Err(SessionMetadataListQueryError::EmptyTitleSearch);
            }
            if query.contains('\0') {
                return Err(SessionMetadataListQueryError::TitleSearchContainsNul);
            }
            total_utf8_bytes = total_utf8_bytes.saturating_add(query.len());
            if total_utf8_bytes > SessionMetadataContent::MAX_TOTAL_UTF8_BYTES {
                return Err(SessionMetadataListQueryError::TotalUtf8BytesExceeded);
            }
        }
        if min_page_size.is_some_and(|minimum| page_size < minimum)
            || max_page_size.is_some_and(|maximum| page_size > maximum)
        {
            return Err(SessionMetadataListQueryError::PageSizeOutOfRange);
        }

        Ok(Self {
            required_tags: canonical_tags,
            title_contains,
            include_archived,
            page_size,
            after_session,
        })
    }

    /// Iterates exact required tags in scalar order.
    pub fn required_tags(&self) -> impl ExactSizeIterator<Item = &str> {
        self.required_tags.iter().map(String::as_str)
    }

    /// Borrows the exact optional case-sensitive title substring.
    pub fn title_contains(&self) -> Option<&str> {
        self.title_contains.as_deref()
    }

    /// Returns whether archived sessions participate in the projection.
    pub const fn include_archived(&self) -> bool {
        self.include_archived
    }

    /// Returns the inclusive bounded maximum result count.
    pub const fn page_size(&self) -> u64 {
        self.page_size
    }

    /// Returns the exclusive keyset cursor.
    pub const fn after_session(&self) -> Option<SessionId> {
        self.after_session
    }
}

/// Why a metadata list query could not be constructed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionMetadataListQueryError {
    /// More required tags were supplied than one filter admits.
    TooManyRequiredTags,
    /// One required tag was empty.
    EmptyTag,
    /// One required tag contained U+0000.
    TagContainsNul,
    /// One required tag exceeded the indexed-string UTF-8 byte bound.
    TagExceedsIndexedUtf8Bytes,
    /// The same exact required tag was supplied more than once.
    DuplicateTag,
    /// A present title substring was empty.
    EmptyTitleSearch,
    /// A present title substring contained U+0000.
    TitleSearchContainsNul,
    /// All filter strings together exceeded the query UTF-8 byte bound.
    TotalUtf8BytesExceeded,
    /// Page size was outside 1 through 100.
    PageSizeOutOfRange,
}

/// One bounded session metadata list item.
///
/// The list projection deliberately carries only the current defaults
/// version, model selection, and dangerous-tool posture — never the current
/// epoch's optional system prompt, whose bounded content is served by the
/// single-session defaults read alone
/// (docs/spec/sessions-and-transcript.md).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionMetadataListItem {
    session: SessionId,
    defaults_version: SessionConfigurationDefaultsVersion,
    model_selection: ModelSelectionRequest,
    dangerous_tool_auto_approval: DangerousToolAutoApproval,
    title: Option<String>,
    tags: Vec<String>,
    archived: bool,
    last_writer: Option<SessionMetadataLastWriter>,
}

impl SessionMetadataListItem {
    /// Projects list fields from a checked metadata snapshot and current
    /// defaults facts. Attributes and the current epoch's system prompt are
    /// deliberately dropped.
    pub fn new(
        snapshot: &SessionMetadataSnapshot,
        defaults_version: SessionConfigurationDefaultsVersion,
        model_selection: ModelSelectionRequest,
        dangerous_tool_auto_approval: DangerousToolAutoApproval,
    ) -> Self {
        Self {
            session: snapshot.session(),
            defaults_version,
            model_selection,
            dangerous_tool_auto_approval,
            title: snapshot.content().title().map(str::to_owned),
            tags: snapshot.content().tags().map(str::to_owned).collect(),
            archived: snapshot.content().archived(),
            last_writer: snapshot.last_writer(),
        }
    }

    /// Returns the summarized session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the current defaults version.
    pub const fn defaults_version(&self) -> SessionConfigurationDefaultsVersion {
        self.defaults_version
    }

    /// Returns the current model-selection request.
    pub const fn model_selection(&self) -> ModelSelectionRequest {
        self.model_selection
    }

    /// Returns the current dangerous blanket-auto posture.
    pub const fn dangerous_tool_auto_approval(&self) -> DangerousToolAutoApproval {
        self.dangerous_tool_auto_approval
    }

    /// Borrows the exact optional title.
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// Iterates exact tags in scalar order.
    pub fn tags(&self) -> impl ExactSizeIterator<Item = &str> {
        self.tags.iter().map(String::as_str)
    }

    /// Returns whether the session is archived.
    pub const fn archived(&self) -> bool {
        self.archived
    }

    /// Returns the optional last-writer stamp.
    pub const fn last_writer(&self) -> Option<SessionMetadataLastWriter> {
        self.last_writer
    }
}

/// One opened, repeatable-read metadata page.
pub trait SessionMetadataPageReader {
    /// Adapter-specific infrastructure or integrity failure.
    type Error;

    /// Yields one item at a time in strict session-identity order.
    fn next_item(
        &mut self,
    ) -> impl Future<Output = Result<Option<SessionMetadataListItem>, Self::Error>> + Send;

    /// Returns the continuation cursor after [`Self::next_item`] yielded
    /// `None`. A caller must exhaust the page before reading this value.
    fn next_after_session(&self) -> Option<SessionId>;
}

/// Application-owned port for opening one metadata list page.
pub trait SessionMetadataLister {
    /// Adapter-specific infrastructure or integrity failure.
    type Error;
    /// The bounded page reader returned for a successful open.
    type Page: SessionMetadataPageReader<Error = Self::Error>;

    /// Opens one repeatable-read page for the exact validated query.
    fn open_session_metadata_page(
        &self,
        query: SessionMetadataListQuery,
    ) -> impl Future<Output = Result<Self::Page, Self::Error>> + Send;
}

/// Coordinates one metadata list-page query.
#[derive(Debug)]
pub struct ListSessionMetadataService<Lister> {
    lister: Lister,
}

impl<Lister> ListSessionMetadataService<Lister> {
    /// Composes the use case with its page-opening port.
    pub const fn new(lister: Lister) -> Self {
        Self { lister }
    }

    /// Returns the lister, primarily for explicit ownership handoff.
    pub fn into_lister(self) -> Lister {
        self.lister
    }
}

impl<Lister> ListSessionMetadataService<Lister>
where
    Lister: SessionMetadataLister,
{
    /// Opens exactly one page without retry or filter rewriting.
    pub async fn execute(
        &self,
        query: SessionMetadataListQuery,
    ) -> Result<Lister::Page, Lister::Error> {
        self.lister.open_session_metadata_page(query).await
    }
}

#[cfg(test)]
mod tests {
    use std::{
        future::{Future, ready},
        pin::pin,
        sync::Mutex,
        task::{Context, Poll, Waker},
    };

    use signalbox_domain::{
        Actor, DirectModelSelection, ModelSelectionRequest, ReplaceSessionMetadataRejectedResult,
        SessionConfigurationDefaults, SessionMetadataUpdatedAt, ToolRequestId,
    };
    use uuid::Uuid;

    use super::{
        DomainReplaceSessionMetadata, DurableCommandId, InvalidDurableCommandId,
        ListSessionMetadataService, LoadSessionMetadataService, ReplaceSessionMetadataOutcome,
        ReplaceSessionMetadataRequest, ReplaceSessionMetadataService,
        ReplaceSessionMetadataTransaction, SessionId, SessionMetadataContent,
        SessionMetadataLastWriter, SessionMetadataListQuery, SessionMetadataListQueryError,
        SessionMetadataLister, SessionMetadataReader, SessionMetadataSnapshot,
    };

    fn command_id(value: u128) -> DurableCommandId {
        DurableCommandId::from_uuid(Uuid::from_u128(value))
    }

    fn session_id(value: u128) -> SessionId {
        SessionId::from_uuid(Uuid::from_u128(value))
    }

    fn tool_request_id(value: u128) -> ToolRequestId {
        ToolRequestId::from_uuid(Uuid::from_u128(value))
    }

    fn metadata(archived: bool) -> SessionMetadataContent {
        SessionMetadataContent::try_new(
            Some(String::from("Planning")),
            vec![String::from("daily")],
            vec![(String::from("run"), String::from("17"))],
            archived,
        )
        .expect("fixture metadata is valid")
    }

    fn defaults() -> SessionConfigurationDefaults {
        SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(
            DirectModelSelection::from_uuid(Uuid::from_u128(9)),
        ))
    }

    fn run_ready<Output>(future: impl Future<Output = Output>) -> Output {
        let waker = Waker::noop();
        let mut context = Context::from_waker(waker);
        let mut future = pin!(future);
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("fake-backed use case must be immediately ready"),
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum FakeError {
        Unavailable,
    }

    #[derive(Debug)]
    struct FakeTransaction {
        observed: Vec<DomainReplaceSessionMetadata>,
        response: Result<ReplaceSessionMetadataOutcome, FakeError>,
    }

    impl ReplaceSessionMetadataTransaction for FakeTransaction {
        type Error = FakeError;

        fn handle(
            &mut self,
            command: DomainReplaceSessionMetadata,
        ) -> impl Future<Output = Result<ReplaceSessionMetadataOutcome, Self::Error>> + Send
        {
            self.observed.push(command);
            ready(self.response.clone())
        }
    }

    /// reserved user-global identifiers fail before transaction
    /// construction and therefore claim no command meaning.
    #[test]
    fn reserved_command_identities_fail_before_transaction_construction() {
        let target = session_id(1);

        assert_eq!(
            ReplaceSessionMetadataRequest::try_new(
                DurableCommandId::from_uuid(Uuid::nil()),
                target,
                metadata(false),
            ),
            Err(InvalidDurableCommandId::Nil)
        );
        assert_eq!(
            ReplaceSessionMetadataRequest::try_new(
                DurableCommandId::from_uuid(Uuid::max()),
                target,
                metadata(false),
            ),
            Err(InvalidDurableCommandId::Max)
        );
    }

    /// process-facing metadata replacement fixes user agency.
    #[test]
    fn replacement_orchestration_fixes_user_actor() {
        let request =
            ReplaceSessionMetadataRequest::try_new(command_id(1), session_id(2), metadata(true))
                .expect("ordinary command identity is admitted");
        let recorded = DomainReplaceSessionMetadata::new(
            request.command_id(),
            request.session(),
            request.replacement().clone(),
        )
        .prepare_session_not_found()
        .into_parts()
        .1;
        let expected = ReplaceSessionMetadataOutcome::Recorded(recorded);
        let mut service = ReplaceSessionMetadataService::new(FakeTransaction {
            observed: Vec::new(),
            response: Ok(expected.clone()),
        });

        let outcome = run_ready(service.execute(request.clone()))
            .expect("fake transaction returns its exact result");

        assert_eq!(outcome, expected);
        let observed = service.into_transaction().observed;
        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0].command_id(), request.command_id());
        assert_eq!(observed[0].session(), request.session());
        assert_eq!(observed[0].actor(), Actor::User);
        assert_eq!(observed[0].replacement(), request.replacement());
    }

    #[test]
    fn replacement_orchestration_retains_tool_actor() {
        let tool_request = tool_request_id(3);
        let request = ReplaceSessionMetadataRequest::try_new_for_tool(
            command_id(1),
            session_id(2),
            tool_request,
            metadata(true),
        )
        .expect("ordinary command identity is admitted");
        let recorded = DomainReplaceSessionMetadata::new_for_tool(
            request.command_id(),
            request.session(),
            tool_request,
            request.replacement().clone(),
        )
        .prepare_session_not_found()
        .into_parts()
        .1;
        let expected = ReplaceSessionMetadataOutcome::Recorded(recorded);
        let mut service = ReplaceSessionMetadataService::new(FakeTransaction {
            observed: Vec::new(),
            response: Ok(expected.clone()),
        });

        let outcome = run_ready(service.execute(request.clone()))
            .expect("fake transaction returns its exact result");

        assert_eq!(outcome, expected);
        let observed = service.into_transaction().observed;
        let [command] = observed.as_slice() else {
            panic!("one tool metadata command crosses the transaction")
        };
        assert_eq!(command.command_id(), request.command_id());
        assert_eq!(command.session(), request.session());
        assert_eq!(command.actor(), request.actor());
        assert_eq!(
            command.actor(),
            Actor::Tool {
                request: tool_request
            }
        );
        assert_eq!(command.replacement(), request.replacement());
    }

    #[test]
    fn replacement_transaction_failure_is_not_retried() {
        let request =
            ReplaceSessionMetadataRequest::try_new(command_id(1), session_id(2), metadata(false))
                .expect("ordinary command identity is admitted");
        let mut service = ReplaceSessionMetadataService::new(FakeTransaction {
            observed: Vec::new(),
            response: Err(FakeError::Unavailable),
        });

        let error =
            run_ready(service.execute(request)).expect_err("transaction failure remains visible");

        assert_eq!(error, FakeError::Unavailable);
        assert_eq!(service.into_transaction().observed.len(), 1);
    }

    #[derive(Debug)]
    struct FakeReader {
        observed: Mutex<Vec<SessionId>>,
        response: Result<Option<SessionMetadataSnapshot>, FakeError>,
    }

    impl SessionMetadataReader for FakeReader {
        type Error = FakeError;

        fn load_session_metadata(
            &self,
            session: SessionId,
        ) -> impl Future<Output = Result<Option<SessionMetadataSnapshot>, Self::Error>> + Send
        {
            self.observed
                .lock()
                .expect("fake reader observation lock is not poisoned")
                .push(session);
            ready(self.response.clone())
        }
    }

    #[test]
    fn current_metadata_read_passes_through_exact_snapshot() {
        let session = session_id(1);
        let expected = SessionMetadataSnapshot::from_recorded_write(
            session,
            metadata(true),
            SessionMetadataLastWriter::new(
                SessionMetadataUpdatedAt::from_unix_micros(17),
                Actor::User,
            ),
        );
        let service = LoadSessionMetadataService::new(FakeReader {
            observed: Mutex::new(Vec::new()),
            response: Ok(Some(expected.clone())),
        });

        let actual = run_ready(service.execute(session))
            .expect("fake reader succeeds")
            .expect("fixture session exists");

        assert_eq!(actual, expected);
        assert_eq!(
            service
                .into_reader()
                .observed
                .into_inner()
                .expect("fake reader observation lock is not poisoned"),
            [session]
        );
    }

    #[test]
    fn default_list_query_selects_first_non_archived_page() {
        let query = SessionMetadataListQuery::default_page(5);

        assert_eq!(
            query.required_tags().collect::<Vec<_>>(),
            Vec::<&str>::new()
        );
        assert_eq!(query.title_contains(), None);
        assert!(!query.include_archived());
        assert_eq!(query.page_size(), 5);
        assert_eq!(query.after_session(), None);
    }

    #[test]
    fn list_query_canonicalizes_required_tag_order() {
        let query = SessionMetadataListQuery::try_new_with_limits(
            vec![String::from("z"), String::from("a")],
            Some(String::from("Plan")),
            true,
            7,
            Some(session_id(9)),
            None,
            Some(2),
            Some(7),
        )
        .expect("valid filters and upper-bound page size are admitted");

        assert_eq!(query.required_tags().collect::<Vec<_>>(), ["a", "z"]);
        assert_eq!(query.title_contains(), Some("Plan"));
        assert!(query.include_archived());
        assert_eq!(query.page_size(), 7);
        assert_eq!(query.after_session(), Some(session_id(9)));
    }

    #[test]
    fn list_query_rejects_out_of_range_page_size() {
        let zero = SessionMetadataListQuery::try_new_with_limits(
            Vec::new(),
            None,
            false,
            1,
            None,
            None,
            Some(2),
            Some(7),
        )
        .expect_err("zero cannot bound a page");
        let too_large = SessionMetadataListQuery::try_new_with_limits(
            Vec::new(),
            None,
            false,
            8,
            None,
            None,
            Some(2),
            Some(7),
        )
        .expect_err("the page cap is inclusive at one hundred");

        assert_eq!(zero, SessionMetadataListQueryError::PageSizeOutOfRange);
        assert_eq!(too_large, SessionMetadataListQueryError::PageSizeOutOfRange);
    }

    #[test]
    fn list_query_rejects_required_tag_cardinality_over_bound() {
        const CONFIGURED_MAX_REQUIRED_TAGS: usize = 2;
        let error = SessionMetadataListQuery::try_new_with_required_tag_limit(
            vec![String::from("tag"); CONFIGURED_MAX_REQUIRED_TAGS + 1],
            None,
            false,
            50,
            None,
            Some(CONFIGURED_MAX_REQUIRED_TAGS),
        )
        .expect_err("one required tag above the cardinality bound is rejected");

        assert_eq!(error, SessionMetadataListQueryError::TooManyRequiredTags);
    }

    #[test]
    fn list_query_rejects_duplicate_required_tag() {
        let error = SessionMetadataListQuery::try_new(
            vec![String::from("same"), String::from("same")],
            None,
            false,
            50,
            None,
        )
        .expect_err("the exact AND set has no duplicates");

        assert_eq!(error, SessionMetadataListQueryError::DuplicateTag);
    }

    #[test]
    fn list_query_accepts_exact_total_utf8_byte_bound() {
        let title = "x".repeat(SessionMetadataContent::MAX_TOTAL_UTF8_BYTES);

        let query =
            SessionMetadataListQuery::try_new(Vec::new(), Some(title.clone()), false, 50, None)
                .expect("the aggregate filter bound is inclusive");

        assert_eq!(query.title_contains(), Some(title.as_str()));
    }

    #[test]
    fn list_query_rejects_total_utf8_bytes_over_bound() {
        let error = SessionMetadataListQuery::try_new(
            Vec::new(),
            Some("x".repeat(SessionMetadataContent::MAX_TOTAL_UTF8_BYTES + 1)),
            false,
            50,
            None,
        )
        .expect_err("one byte above the aggregate filter bound is rejected");

        assert_eq!(error, SessionMetadataListQueryError::TotalUtf8BytesExceeded);
    }

    #[test]
    fn list_query_rejects_required_tag_over_indexed_utf8_byte_bound() {
        let error = SessionMetadataListQuery::try_new(
            vec!["x".repeat(SessionMetadataContent::MAX_INDEXED_UTF8_BYTES + 1)],
            None,
            false,
            50,
            None,
        )
        .expect_err("an oversized indexed required tag is rejected");

        assert_eq!(
            error,
            SessionMetadataListQueryError::TagExceedsIndexedUtf8Bytes
        );
    }

    #[derive(Debug)]
    struct FakePage;

    impl super::SessionMetadataPageReader for FakePage {
        type Error = FakeError;

        fn next_item(
            &mut self,
        ) -> impl Future<Output = Result<Option<super::SessionMetadataListItem>, Self::Error>> + Send
        {
            ready(Ok(None))
        }

        fn next_after_session(&self) -> Option<SessionId> {
            None
        }
    }

    #[derive(Debug)]
    struct FakeLister {
        observed: Mutex<Vec<SessionMetadataListQuery>>,
        fail: bool,
    }

    impl SessionMetadataLister for FakeLister {
        type Error = FakeError;
        type Page = FakePage;

        fn open_session_metadata_page(
            &self,
            query: SessionMetadataListQuery,
        ) -> impl Future<Output = Result<Self::Page, Self::Error>> + Send {
            self.observed
                .lock()
                .expect("fake lister observation lock is not poisoned")
                .push(query);
            ready(if self.fail {
                Err(FakeError::Unavailable)
            } else {
                Ok(FakePage)
            })
        }
    }

    #[test]
    fn list_service_opens_exact_query_once() {
        let query = SessionMetadataListQuery::default_page(5);
        let service = ListSessionMetadataService::new(FakeLister {
            observed: Mutex::new(Vec::new()),
            fail: false,
        });

        let _page = run_ready(service.execute(query.clone())).expect("fake lister opens the page");

        assert_eq!(
            service
                .into_lister()
                .observed
                .into_inner()
                .expect("fake lister observation lock is not poisoned"),
            [query]
        );
    }

    #[test]
    fn list_service_returns_open_failure_without_retry() {
        let service = ListSessionMetadataService::new(FakeLister {
            observed: Mutex::new(Vec::new()),
            fail: true,
        });

        let error = run_ready(service.execute(SessionMetadataListQuery::default_page(5)))
            .expect_err("open failure remains visible");

        assert_eq!(error, FakeError::Unavailable);
        assert_eq!(
            service
                .into_lister()
                .observed
                .into_inner()
                .expect("fake lister observation lock is not poisoned")
                .len(),
            1
        );
    }

    #[test]
    fn list_item_omits_attributes_but_retains_summary_fields() {
        let session = session_id(1);
        let snapshot = SessionMetadataSnapshot::from_recorded_write(
            session,
            metadata(true),
            SessionMetadataLastWriter::new(
                SessionMetadataUpdatedAt::from_unix_micros(17),
                Actor::User,
            ),
        );
        let version = signalbox_domain::SessionConfigurationDefaultsVersion::try_from_u64(3)
            .expect("fixture version is positive");
        let item = super::SessionMetadataListItem::new(
            &snapshot,
            version,
            defaults().model(),
            defaults().dangerous_tool_auto_approval(),
        );

        assert_eq!(item.session(), session);
        assert_eq!(item.defaults_version(), version);
        assert_eq!(item.model_selection(), defaults().model());
        assert_eq!(
            item.dangerous_tool_auto_approval(),
            defaults().dangerous_tool_auto_approval()
        );
        assert_eq!(item.title(), snapshot.content().title());
        assert_eq!(
            item.tags().collect::<Vec<_>>(),
            snapshot.content().tags().collect::<Vec<_>>()
        );
        assert!(item.archived());
        assert_eq!(item.last_writer(), snapshot.last_writer());
    }

    #[test]
    fn recorded_missing_session_rejection_passes_through() {
        let request =
            ReplaceSessionMetadataRequest::try_new(command_id(1), session_id(2), metadata(false))
                .expect("ordinary command identity is admitted");
        let command = DomainReplaceSessionMetadata::new(
            request.command_id(),
            request.session(),
            request.replacement().clone(),
        );
        let recorded = command.prepare_session_not_found().into_parts().1;
        assert!(matches!(
            recorded,
            signalbox_domain::ReplaceSessionMetadataResult::Rejected(
                ReplaceSessionMetadataRejectedResult::SessionNotFound(_)
            )
        ));
        let expected = ReplaceSessionMetadataOutcome::Recorded(recorded);
        let mut service = ReplaceSessionMetadataService::new(FakeTransaction {
            observed: Vec::new(),
            response: Ok(expected.clone()),
        });

        let actual =
            run_ready(service.execute(request)).expect("recorded rejection remains terminal");

        assert_eq!(actual, expected);
        assert_eq!(service.into_transaction().observed.len(), 1);
    }
}

//! Replaceable organizational metadata for one session.
//!
//! `docs/spec/sessions-and-transcript.md` keeps title, tags, attributes, and
//! archive state outside the conversational [`crate::Session`] aggregate. This
//! module owns the exact metadata value, its last-writer stamp, the canonical
//! durable replacement command, and fail-closed receipt reconstitution. It
//! performs no lookup, persistence, command claim, clock read, or
//! acknowledgement.

use std::collections::{BTreeMap, BTreeSet};

use crate::{Actor, DurableCommandId, SessionId, ToolRequestId};

/// One complete replaceable organizational metadata value.
///
/// Strings retain exact Unicode scalar order. Construction rejects U+0000,
/// which PostgreSQL text cannot represent, and rejects empty titles, tags, and
/// attribute keys. The complete snapshot and each indexed string are bounded.
/// Whitespace-only values remain valid and no normalization, trimming, or case
/// folding occurs.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SessionMetadataContent {
    title: Option<String>,
    tags: BTreeSet<String>,
    attributes: BTreeMap<String, String>,
    archived: bool,
}

impl SessionMetadataContent {
    /// Maximum total UTF-8 bytes in one complete metadata snapshot.
    pub const MAX_TOTAL_UTF8_BYTES: usize = 262_144;

    /// Maximum UTF-8 bytes in one indexed metadata tag or attribute key.
    pub const MAX_INDEXED_UTF8_BYTES: usize = 1_024;

    /// Constructs the canonical empty, non-archived metadata value.
    pub fn empty() -> Self {
        Self {
            title: None,
            tags: BTreeSet::new(),
            attributes: BTreeMap::new(),
            archived: false,
        }
    }

    /// Validates and canonicalizes one complete replacement value.
    ///
    /// Input order has no meaning. Duplicate tags or attribute keys are
    /// rejected rather than silently deduplicated or overwritten.
    pub fn try_new(
        title: Option<String>,
        tags: Vec<String>,
        attributes: Vec<(String, String)>,
        archived: bool,
    ) -> Result<Self, SessionMetadataContentError> {
        Self::try_new_with_count_limits(title, tags, attributes, archived, None, None)
    }

    /// Validates one replacement against deployment-owned count policies.
    pub fn try_new_with_count_limits(
        title: Option<String>,
        tags: Vec<String>,
        attributes: Vec<(String, String)>,
        archived: bool,
        max_tags: Option<usize>,
        max_attributes: Option<usize>,
    ) -> Result<Self, SessionMetadataContentError> {
        if max_tags.is_some_and(|limit| tags.len() > limit) {
            return Err(SessionMetadataContentError::TooManyTags);
        }
        if max_attributes.is_some_and(|limit| attributes.len() > limit) {
            return Err(SessionMetadataContentError::TooManyAttributes);
        }

        let mut total_utf8_bytes = 0;
        if let Some(value) = title.as_deref() {
            validate_nonempty(value, SessionMetadataContentError::EmptyTitle)?;
            validate_storable(value, SessionMetadataContentError::TitleContainsNul)?;
            add_utf8_bytes(&mut total_utf8_bytes, value)?;
        }

        let mut canonical_tags = BTreeSet::new();
        for tag in tags {
            validate_nonempty(&tag, SessionMetadataContentError::EmptyTag)?;
            validate_storable(&tag, SessionMetadataContentError::TagContainsNul)?;
            validate_indexed(
                &tag,
                SessionMetadataContentError::TagExceedsIndexedUtf8Bytes,
            )?;
            add_utf8_bytes(&mut total_utf8_bytes, &tag)?;
            if !canonical_tags.insert(tag) {
                return Err(SessionMetadataContentError::DuplicateTag);
            }
        }

        let mut canonical_attributes = BTreeMap::new();
        for (key, value) in attributes {
            validate_nonempty(&key, SessionMetadataContentError::EmptyAttributeKey)?;
            validate_storable(&key, SessionMetadataContentError::AttributeKeyContainsNul)?;
            validate_indexed(
                &key,
                SessionMetadataContentError::AttributeKeyExceedsIndexedUtf8Bytes,
            )?;
            add_utf8_bytes(&mut total_utf8_bytes, &key)?;
            validate_storable(
                &value,
                SessionMetadataContentError::AttributeValueContainsNul,
            )?;
            add_utf8_bytes(&mut total_utf8_bytes, &value)?;
            if canonical_attributes.insert(key, value).is_some() {
                return Err(SessionMetadataContentError::DuplicateAttributeKey);
            }
        }

        Ok(Self {
            title,
            tags: canonical_tags,
            attributes: canonical_attributes,
            archived,
        })
    }

    /// Borrows the exact optional title.
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// Iterates tags in exact scalar order.
    pub fn tags(&self) -> impl ExactSizeIterator<Item = &str> {
        self.tags.iter().map(String::as_str)
    }

    /// Iterates attribute keys and values in exact key scalar order.
    pub fn attributes(&self) -> impl ExactSizeIterator<Item = (&str, &str)> {
        self.attributes
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
    }

    /// Returns whether this session is omitted from the default view.
    pub const fn archived(&self) -> bool {
        self.archived
    }
}

fn validate_nonempty(
    value: &str,
    failure: SessionMetadataContentError,
) -> Result<(), SessionMetadataContentError> {
    if value.is_empty() {
        Err(failure)
    } else {
        Ok(())
    }
}

fn validate_storable(
    value: &str,
    failure: SessionMetadataContentError,
) -> Result<(), SessionMetadataContentError> {
    if value.contains('\0') {
        Err(failure)
    } else {
        Ok(())
    }
}

fn validate_indexed(
    value: &str,
    failure: SessionMetadataContentError,
) -> Result<(), SessionMetadataContentError> {
    if value.len() > SessionMetadataContent::MAX_INDEXED_UTF8_BYTES {
        Err(failure)
    } else {
        Ok(())
    }
}

fn add_utf8_bytes(total: &mut usize, value: &str) -> Result<(), SessionMetadataContentError> {
    *total = total.saturating_add(value.len());
    if *total > SessionMetadataContent::MAX_TOTAL_UTF8_BYTES {
        Err(SessionMetadataContentError::TotalUtf8BytesExceeded)
    } else {
        Ok(())
    }
}

/// Why a complete metadata value could not be constructed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionMetadataContentError {
    /// A present title was empty.
    EmptyTitle,
    /// A present title contained U+0000.
    TitleContainsNul,
    /// The caller supplied more tags than one snapshot admits.
    TooManyTags,
    /// One tag was empty.
    EmptyTag,
    /// One tag contained U+0000.
    TagContainsNul,
    /// One tag exceeded the indexed-string UTF-8 byte bound.
    TagExceedsIndexedUtf8Bytes,
    /// The caller supplied the same exact tag more than once.
    DuplicateTag,
    /// The caller supplied more attributes than one snapshot admits.
    TooManyAttributes,
    /// One attribute key was empty.
    EmptyAttributeKey,
    /// One attribute key contained U+0000.
    AttributeKeyContainsNul,
    /// One attribute key exceeded the indexed-string UTF-8 byte bound.
    AttributeKeyExceedsIndexedUtf8Bytes,
    /// One attribute value contained U+0000.
    AttributeValueContainsNul,
    /// The caller supplied the same exact attribute key more than once.
    DuplicateAttributeKey,
    /// All metadata strings together exceeded the snapshot UTF-8 byte bound.
    TotalUtf8BytesExceeded,
}

/// Database statement time sampled after the metadata writer lock.
///
/// The integer is nonnegative microseconds since the Unix epoch. It is
/// server-derived result evidence, not caller intent and not an ordering token
/// for conversational state.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SessionMetadataUpdatedAt(u64);

impl SessionMetadataUpdatedAt {
    /// Constructs the exact nonnegative Unix-microsecond value.
    pub const fn from_unix_micros(value: u64) -> Self {
        Self(value)
    }

    /// Returns the exact nonnegative Unix-microsecond value.
    pub const fn as_unix_micros(self) -> u64 {
        self.0
    }
}

/// The most recent committed metadata writer.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SessionMetadataLastWriter {
    updated_at: SessionMetadataUpdatedAt,
    actor: Actor,
}

impl SessionMetadataLastWriter {
    /// Pairs the post-lock database statement timestamp with its writer.
    pub const fn new(updated_at: SessionMetadataUpdatedAt, actor: Actor) -> Self {
        Self { updated_at, actor }
    }

    /// Returns the database statement time sampled after the writer lock.
    pub const fn updated_at(self) -> SessionMetadataUpdatedAt {
        self.updated_at
    }

    /// Returns the actor attributed by the replacement command.
    pub const fn actor(self) -> Actor {
        self.actor
    }
}

/// One complete current metadata read projection.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SessionMetadataSnapshot {
    session: SessionId,
    content: SessionMetadataContent,
    last_writer: Option<SessionMetadataLastWriter>,
}

impl SessionMetadataSnapshot {
    /// Constructs the canonical projection for a session with no metadata row.
    pub fn initial(session: SessionId) -> Self {
        Self {
            session,
            content: SessionMetadataContent::empty(),
            last_writer: None,
        }
    }

    /// Constructs a projection from one completely decoded metadata row and
    /// its normalized satellites.
    pub fn from_recorded_write(
        session: SessionId,
        content: SessionMetadataContent,
        last_writer: SessionMetadataLastWriter,
    ) -> Self {
        Self {
            session,
            content,
            last_writer: Some(last_writer),
        }
    }

    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Borrows the complete current metadata content.
    pub const fn content(&self) -> &SessionMetadataContent {
        &self.content
    }

    /// Returns the last writer, absent only before the first metadata write.
    pub const fn last_writer(&self) -> Option<SessionMetadataLastWriter> {
        self.last_writer
    }
}

/// The canonical durable command replacing one complete metadata snapshot.
///
/// Structural equality and hashing exclude `command_id` and cover every other
/// caller-supplied semantic field. Construction admits only the user boundary
/// and execution of one exact tool request; callers cannot supply an arbitrary
/// actor.
#[derive(Clone, Debug)]
pub struct ReplaceSessionMetadata {
    command_id: DurableCommandId,
    session: SessionId,
    actor: Actor,
    replacement: SessionMetadataContent,
}

impl ReplaceSessionMetadata {
    /// Constructs the complete canonical user payload.
    pub const fn new(
        command_id: DurableCommandId,
        session: SessionId,
        replacement: SessionMetadataContent,
    ) -> Self {
        Self {
            command_id,
            session,
            actor: Actor::User,
            replacement,
        }
    }

    /// Constructs a replacement initiated by execution of one exact tool
    /// request.
    pub const fn new_for_tool(
        command_id: DurableCommandId,
        session: SessionId,
        request: ToolRequestId,
        replacement: SessionMetadataContent,
    ) -> Self {
        Self {
            command_id,
            session,
            actor: Actor::Tool { request },
            replacement,
        }
    }

    /// Returns the user-global durable command identity.
    pub const fn command_id(&self) -> DurableCommandId {
        self.command_id
    }

    /// Returns the target session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the initiating agency carried by this command.
    pub const fn actor(&self) -> Actor {
        self.actor
    }

    /// Borrows the complete replacement content.
    pub const fn replacement(&self) -> &SessionMetadataContent {
        &self.replacement
    }

    /// Prepares the authoritative rejection for a target proven absent.
    ///
    /// This value is a pre-commit candidate and proves no command claim.
    pub fn prepare_session_not_found(self) -> PreparedReplaceSessionMetadata {
        let session = self.session;
        PreparedReplaceSessionMetadata {
            command: self,
            result: ReplaceSessionMetadataResult::Rejected(
                ReplaceSessionMetadataRejectedResult::SessionNotFound(
                    ReplaceSessionMetadataSessionNotFound { session },
                ),
            ),
        }
    }

    /// Prepares the applied result using transaction-owned clock evidence.
    ///
    /// This value is a pre-commit candidate and proves no write or command
    /// claim.
    pub fn prepare_applied(
        self,
        updated_at: SessionMetadataUpdatedAt,
    ) -> PreparedReplaceSessionMetadata {
        let snapshot = SessionMetadataSnapshot::from_recorded_write(
            self.session,
            self.replacement.clone(),
            SessionMetadataLastWriter::new(updated_at, self.actor()),
        );
        PreparedReplaceSessionMetadata {
            command: self,
            result: ReplaceSessionMetadataResult::Applied(ReplaceSessionMetadataAppliedResult {
                snapshot,
            }),
        }
    }
}

impl PartialEq for ReplaceSessionMetadata {
    fn eq(&self, other: &Self) -> bool {
        self.session == other.session
            && self.actor == other.actor
            && self.replacement == other.replacement
    }
}

impl Eq for ReplaceSessionMetadata {}

impl std::hash::Hash for ReplaceSessionMetadata {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.session.hash(state);
        self.actor.hash(state);
        self.replacement.hash(state);
    }
}

/// The terminal typed result for one metadata-replacement command.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum ReplaceSessionMetadataResult {
    /// The complete replacement and last-writer stamp committed.
    Applied(ReplaceSessionMetadataAppliedResult),
    /// Authoritative state rejected the command without changing metadata.
    Rejected(ReplaceSessionMetadataRejectedResult),
}

/// The terminal applied metadata-replacement result.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ReplaceSessionMetadataAppliedResult {
    snapshot: SessionMetadataSnapshot,
}

impl ReplaceSessionMetadataAppliedResult {
    /// Borrows the complete committed snapshot.
    pub const fn snapshot(&self) -> &SessionMetadataSnapshot {
        &self.snapshot
    }
}

/// A closed authoritative rejection result for metadata replacement.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ReplaceSessionMetadataRejectedResult {
    /// The target session did not exist in the handling transaction.
    SessionNotFound(ReplaceSessionMetadataSessionNotFound),
}

/// Typed facts for an authoritative missing-session rejection.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ReplaceSessionMetadataSessionNotFound {
    session: SessionId,
}

impl ReplaceSessionMetadataSessionNotFound {
    /// Returns the absent target session.
    pub const fn session(self) -> SessionId {
        self.session
    }
}

/// One sealed pre-commit handling candidate.
#[derive(Clone, Debug)]
pub struct PreparedReplaceSessionMetadata {
    command: ReplaceSessionMetadata,
    result: ReplaceSessionMetadataResult,
}

impl PreparedReplaceSessionMetadata {
    /// Borrows the exact canonical command.
    pub const fn command(&self) -> &ReplaceSessionMetadata {
        &self.command
    }

    /// Borrows the exact terminal result.
    pub const fn result(&self) -> &ReplaceSessionMetadataResult {
        &self.result
    }

    /// Consumes the candidate into its correlated transaction inputs.
    pub fn into_parts(self) -> (ReplaceSessionMetadata, ReplaceSessionMetadataResult) {
        (self.command, self.result)
    }
}

#[derive(Clone, Copy, Debug)]
enum ReplaceSessionMetadataReconstitutionFacts {
    Applied {
        result_session: SessionId,
        result_updated_at: SessionMetadataUpdatedAt,
        result_actor: Actor,
    },
    RejectedSessionNotFound {
        result_session: SessionId,
    },
}

/// Complete checked inputs for reconstructing one recorded metadata command.
#[derive(Clone, Debug)]
pub struct ReplaceSessionMetadataReconstitutionInput {
    command: ReplaceSessionMetadata,
    command_actor: Actor,
    facts: ReplaceSessionMetadataReconstitutionFacts,
}

impl ReplaceSessionMetadataReconstitutionInput {
    /// Supplies the complete applied-result facts.
    pub const fn applied(
        command: ReplaceSessionMetadata,
        command_actor: Actor,
        result_session: SessionId,
        result_updated_at: SessionMetadataUpdatedAt,
        result_actor: Actor,
    ) -> Self {
        Self {
            command,
            command_actor,
            facts: ReplaceSessionMetadataReconstitutionFacts::Applied {
                result_session,
                result_updated_at,
                result_actor,
            },
        }
    }

    /// Supplies the recorded target for a missing-session rejection.
    pub const fn rejected_session_not_found(
        command: ReplaceSessionMetadata,
        command_actor: Actor,
        result_session: SessionId,
    ) -> Self {
        Self {
            command,
            command_actor,
            facts: ReplaceSessionMetadataReconstitutionFacts::RejectedSessionNotFound {
                result_session,
            },
        }
    }

    /// Borrows the reconstructed canonical command.
    pub const fn command(&self) -> &ReplaceSessionMetadata {
        &self.command
    }

    /// Reconstructs the complete recorded handling without I/O or effects.
    pub fn reconstitute(
        self,
    ) -> Result<ReconstitutedReplaceSessionMetadata, ReplaceSessionMetadataReconstitutionError>
    {
        let failure = if self.command_actor != self.command.actor() {
            Some(ReplaceSessionMetadataReconstitutionFailure::CommandActorMismatch)
        } else {
            match self.facts {
                ReplaceSessionMetadataReconstitutionFacts::Applied { result_session, .. }
                    if result_session != self.command.session =>
                {
                    Some(ReplaceSessionMetadataReconstitutionFailure::ResultSessionMismatch)
                }
                ReplaceSessionMetadataReconstitutionFacts::Applied { result_actor, .. }
                    if result_actor != self.command.actor() =>
                {
                    Some(ReplaceSessionMetadataReconstitutionFailure::ResultActorMismatch)
                }
                ReplaceSessionMetadataReconstitutionFacts::RejectedSessionNotFound {
                    result_session,
                } if result_session != self.command.session => {
                    Some(ReplaceSessionMetadataReconstitutionFailure::ResultSessionMismatch)
                }
                _ => None,
            }
        };
        if let Some(failure) = failure {
            return Err(ReplaceSessionMetadataReconstitutionError {
                input: Box::new(self),
                failure,
            });
        }

        let result = match self.facts {
            ReplaceSessionMetadataReconstitutionFacts::Applied {
                result_session,
                result_updated_at,
                result_actor,
            } => ReplaceSessionMetadataResult::Applied(ReplaceSessionMetadataAppliedResult {
                snapshot: SessionMetadataSnapshot::from_recorded_write(
                    result_session,
                    self.command.replacement.clone(),
                    SessionMetadataLastWriter::new(result_updated_at, result_actor),
                ),
            }),
            ReplaceSessionMetadataReconstitutionFacts::RejectedSessionNotFound {
                result_session,
            } => ReplaceSessionMetadataResult::Rejected(
                ReplaceSessionMetadataRejectedResult::SessionNotFound(
                    ReplaceSessionMetadataSessionNotFound {
                        session: result_session,
                    },
                ),
            ),
        };

        Ok(ReconstitutedReplaceSessionMetadata {
            command: self.command,
            result,
        })
    }
}

/// Why typed durable facts cannot reconstruct a recorded replacement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplaceSessionMetadataReconstitutionFailure {
    /// The independently recorded actor differs from the canonical command.
    CommandActorMismatch,
    /// The terminal result names a different target session.
    ResultSessionMismatch,
    /// The applied stamp names a different actor from the command.
    ResultActorMismatch,
}

/// A failed reconstitution retaining every typed input unchanged.
#[derive(Clone, Debug)]
pub struct ReplaceSessionMetadataReconstitutionError {
    input: Box<ReplaceSessionMetadataReconstitutionInput>,
    failure: ReplaceSessionMetadataReconstitutionFailure,
}

impl ReplaceSessionMetadataReconstitutionError {
    /// Returns why the complete projection could not be reconstructed.
    pub const fn failure(&self) -> ReplaceSessionMetadataReconstitutionFailure {
        self.failure
    }

    /// Borrows the complete unchanged input.
    pub const fn input(&self) -> &ReplaceSessionMetadataReconstitutionInput {
        &self.input
    }

    /// Returns the complete unchanged input and failure.
    pub fn into_parts(
        self,
    ) -> (
        ReplaceSessionMetadataReconstitutionInput,
        ReplaceSessionMetadataReconstitutionFailure,
    ) {
        (*self.input, self.failure)
    }
}

/// One complete recorded replacement reconstructed from matching facts.
#[derive(Clone, Debug)]
pub struct ReconstitutedReplaceSessionMetadata {
    command: ReplaceSessionMetadata,
    result: ReplaceSessionMetadataResult,
}

impl ReconstitutedReplaceSessionMetadata {
    /// Borrows the reconstructed canonical command.
    pub const fn command(&self) -> &ReplaceSessionMetadata {
        &self.command
    }

    /// Borrows the reconstructed terminal result.
    pub const fn result(&self) -> &ReplaceSessionMetadataResult {
        &self.result
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::hash_map::DefaultHasher,
        hash::{Hash, Hasher},
    };

    use uuid::Uuid;

    use super::{
        ReplaceSessionMetadata, ReplaceSessionMetadataReconstitutionFailure,
        ReplaceSessionMetadataReconstitutionInput, ReplaceSessionMetadataRejectedResult,
        ReplaceSessionMetadataResult, SessionMetadataContent, SessionMetadataContentError,
        SessionMetadataLastWriter, SessionMetadataSnapshot, SessionMetadataUpdatedAt,
    };
    use crate::{Actor, DurableCommandId, SessionId, ToolRequestId};

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
            vec![String::from("daily"), String::from("work")],
            vec![
                (String::from("run"), String::from("17")),
                (String::from("trigger"), String::new()),
            ],
            archived,
        )
        .expect("fixture metadata is valid")
    }

    fn command_hash(command: &ReplaceSessionMetadata) -> u64 {
        let mut state = DefaultHasher::new();
        command.hash(&mut state);
        state.finish()
    }

    #[test]
    fn empty_metadata_has_no_organizational_values() {
        let content = SessionMetadataContent::empty();

        assert_eq!(content.title(), None);
        assert_eq!(content.tags().collect::<Vec<_>>(), Vec::<&str>::new());
        assert_eq!(
            content.attributes().collect::<Vec<_>>(),
            Vec::<(&str, &str)>::new()
        );
        assert!(!content.archived());
    }

    #[test]
    fn metadata_canonicalizes_tag_set_order() {
        let first = SessionMetadataContent::try_new(
            Some(String::from("  title  ")),
            vec![String::from("z"), String::from("a")],
            Vec::new(),
            true,
        )
        .expect("exact nonempty metadata is valid");
        let second = SessionMetadataContent::try_new(
            Some(String::from("  title  ")),
            vec![String::from("a"), String::from("z")],
            Vec::new(),
            true,
        )
        .expect("reordered exact metadata is valid");

        assert_eq!(first, second);
        assert_eq!(first.tags().collect::<Vec<_>>(), ["a", "z"]);
    }

    #[test]
    fn metadata_canonicalizes_attribute_map_order() {
        let first = SessionMetadataContent::try_new(
            None,
            Vec::new(),
            vec![
                (String::from("z"), String::from("last")),
                (String::from("a"), String::new()),
            ],
            false,
        )
        .expect("exact attributes are valid");
        let second = SessionMetadataContent::try_new(
            None,
            Vec::new(),
            vec![
                (String::from("a"), String::new()),
                (String::from("z"), String::from("last")),
            ],
            false,
        )
        .expect("reordered exact metadata is valid");

        assert_eq!(first, second);
        assert_eq!(
            first.attributes().collect::<Vec<_>>(),
            [("a", ""), ("z", "last")]
        );
    }

    #[test]
    fn metadata_rejects_tag_cardinality_over_bound() {
        let error = SessionMetadataContent::try_new_with_count_limits(
            None,
            vec![String::from("tag"), String::from("other")],
            Vec::new(),
            false,
            Some(1),
            None,
        )
        .expect_err("one tag above the cardinality bound is rejected");

        assert_eq!(error, SessionMetadataContentError::TooManyTags);
    }

    #[test]
    fn metadata_rejects_attribute_cardinality_over_bound() {
        let error = SessionMetadataContent::try_new_with_count_limits(
            None,
            Vec::new(),
            vec![
                (String::from("first"), String::new()),
                (String::from("second"), String::new()),
            ],
            false,
            None,
            Some(1),
        )
        .expect_err("one attribute above the cardinality bound is rejected");

        assert_eq!(error, SessionMetadataContentError::TooManyAttributes);
    }

    #[test]
    fn duplicate_tag_is_rejected() {
        let error = SessionMetadataContent::try_new(
            None,
            vec![String::from("same"), String::from("same")],
            Vec::new(),
            false,
        )
        .expect_err("an exact set cannot contain a duplicate");

        assert_eq!(error, SessionMetadataContentError::DuplicateTag);
    }

    #[test]
    fn duplicate_attribute_key_is_rejected() {
        let error = SessionMetadataContent::try_new(
            None,
            Vec::new(),
            vec![
                (String::from("same"), String::from("first")),
                (String::from("same"), String::from("second")),
            ],
            false,
        )
        .expect_err("an exact map cannot select an implicit winner");

        assert_eq!(error, SessionMetadataContentError::DuplicateAttributeKey);
    }

    #[test]
    fn empty_title_is_rejected() {
        let error =
            SessionMetadataContent::try_new(Some(String::new()), Vec::new(), Vec::new(), false)
                .expect_err("a present title is nonempty");

        assert_eq!(error, SessionMetadataContentError::EmptyTitle);
    }

    #[test]
    fn empty_tag_is_rejected() {
        let error = SessionMetadataContent::try_new(None, vec![String::new()], Vec::new(), false)
            .expect_err("a tag is nonempty");

        assert_eq!(error, SessionMetadataContentError::EmptyTag);
    }

    #[test]
    fn empty_attribute_key_is_rejected() {
        let error = SessionMetadataContent::try_new(
            None,
            Vec::new(),
            vec![(String::new(), String::from("value"))],
            false,
        )
        .expect_err("an attribute key is nonempty");

        assert_eq!(error, SessionMetadataContentError::EmptyAttributeKey);
    }

    #[test]
    fn nul_title_is_rejected() {
        let error = SessionMetadataContent::try_new(
            Some(String::from("bad\0title")),
            Vec::new(),
            Vec::new(),
            false,
        )
        .expect_err("a title must fit PostgreSQL text");

        assert_eq!(error, SessionMetadataContentError::TitleContainsNul);
    }

    #[test]
    fn nul_tag_is_rejected() {
        let error = SessionMetadataContent::try_new(
            None,
            vec![String::from("bad\0tag")],
            Vec::new(),
            false,
        )
        .expect_err("a tag must fit PostgreSQL text");

        assert_eq!(error, SessionMetadataContentError::TagContainsNul);
    }

    #[test]
    fn nul_attribute_key_is_rejected() {
        let error = SessionMetadataContent::try_new(
            None,
            Vec::new(),
            vec![(String::from("bad\0key"), String::new())],
            false,
        )
        .expect_err("an attribute key must fit PostgreSQL text");

        assert_eq!(error, SessionMetadataContentError::AttributeKeyContainsNul);
    }

    #[test]
    fn nul_attribute_value_is_rejected() {
        let error = SessionMetadataContent::try_new(
            None,
            Vec::new(),
            vec![(String::from("key"), String::from("bad\0value"))],
            false,
        )
        .expect_err("an attribute value must fit PostgreSQL text");

        assert_eq!(
            error,
            SessionMetadataContentError::AttributeValueContainsNul
        );
    }

    #[test]
    fn metadata_accepts_exact_total_utf8_byte_bound() {
        let title = "x".repeat(SessionMetadataContent::MAX_TOTAL_UTF8_BYTES);

        let content =
            SessionMetadataContent::try_new(Some(title.clone()), Vec::new(), Vec::new(), false)
                .expect("the aggregate byte bound is inclusive");

        assert_eq!(content.title(), Some(title.as_str()));
    }

    #[test]
    fn metadata_rejects_total_utf8_bytes_over_bound() {
        let error = SessionMetadataContent::try_new(
            Some("x".repeat(SessionMetadataContent::MAX_TOTAL_UTF8_BYTES + 1)),
            Vec::new(),
            Vec::new(),
            false,
        )
        .expect_err("one byte above the aggregate bound is rejected");

        assert_eq!(error, SessionMetadataContentError::TotalUtf8BytesExceeded);
    }

    #[test]
    fn metadata_rejects_tag_over_indexed_utf8_byte_bound() {
        let error = SessionMetadataContent::try_new(
            None,
            vec!["x".repeat(SessionMetadataContent::MAX_INDEXED_UTF8_BYTES + 1)],
            Vec::new(),
            false,
        )
        .expect_err("an oversized indexed tag is rejected");

        assert_eq!(
            error,
            SessionMetadataContentError::TagExceedsIndexedUtf8Bytes
        );
    }

    #[test]
    fn metadata_rejects_attribute_key_over_indexed_utf8_byte_bound() {
        let error = SessionMetadataContent::try_new(
            None,
            Vec::new(),
            vec![(
                "x".repeat(SessionMetadataContent::MAX_INDEXED_UTF8_BYTES + 1),
                String::new(),
            )],
            false,
        )
        .expect_err("an oversized indexed attribute key is rejected");

        assert_eq!(
            error,
            SessionMetadataContentError::AttributeKeyExceedsIndexedUtf8Bytes
        );
    }

    #[test]
    fn initial_snapshot_has_no_last_writer() {
        let session = session_id(1);
        let snapshot = SessionMetadataSnapshot::initial(session);

        assert_eq!(snapshot.session(), session);
        assert_eq!(snapshot.content(), &SessionMetadataContent::empty());
        assert_eq!(snapshot.last_writer(), None);
    }

    /// S25: archive is metadata on the same durable session identity.
    #[test]
    fn recorded_snapshot_preserves_identity_and_archive_state() {
        let session = session_id(1);
        let content = metadata(true);
        let writer = SessionMetadataLastWriter::new(
            SessionMetadataUpdatedAt::from_unix_micros(17),
            Actor::User,
        );
        let snapshot =
            SessionMetadataSnapshot::from_recorded_write(session, content.clone(), writer);

        assert_eq!(snapshot.session(), session);
        assert_eq!(snapshot.content(), &content);
        assert!(snapshot.content().archived());
        assert_eq!(snapshot.last_writer(), Some(writer));
    }

    /// comparison equality excludes only durable command identity and
    /// treats set/map input order canonically.
    #[test]
    fn command_equality_covers_canonical_payload_not_identity() {
        let first = ReplaceSessionMetadata::new(command_id(1), session_id(2), metadata(false));
        let another_identity =
            ReplaceSessionMetadata::new(command_id(3), session_id(2), metadata(false));
        let another_session =
            ReplaceSessionMetadata::new(command_id(1), session_id(4), metadata(false));
        let another_title = ReplaceSessionMetadata::new(
            command_id(1),
            session_id(2),
            SessionMetadataContent::try_new(
                Some(String::from("Other")),
                vec![String::from("daily"), String::from("work")],
                vec![
                    (String::from("run"), String::from("17")),
                    (String::from("trigger"), String::new()),
                ],
                false,
            )
            .expect("comparison fixture is valid"),
        );
        let another_tags = ReplaceSessionMetadata::new(
            command_id(1),
            session_id(2),
            SessionMetadataContent::try_new(
                Some(String::from("Planning")),
                vec![String::from("daily"), String::from("other")],
                vec![
                    (String::from("run"), String::from("17")),
                    (String::from("trigger"), String::new()),
                ],
                false,
            )
            .expect("comparison fixture is valid"),
        );
        let another_attributes = ReplaceSessionMetadata::new(
            command_id(1),
            session_id(2),
            SessionMetadataContent::try_new(
                Some(String::from("Planning")),
                vec![String::from("daily"), String::from("work")],
                vec![
                    (String::from("run"), String::from("18")),
                    (String::from("trigger"), String::new()),
                ],
                false,
            )
            .expect("comparison fixture is valid"),
        );
        let another_archive_state =
            ReplaceSessionMetadata::new(command_id(1), session_id(2), metadata(true));

        assert_eq!(first, another_identity);
        assert_eq!(command_hash(&first), command_hash(&another_identity));
        assert_ne!(first, another_session);
        assert_ne!(first, another_title);
        assert_ne!(first, another_tags);
        assert_ne!(first, another_attributes);
        assert_ne!(first, another_archive_state);
    }

    #[test]
    fn metadata_command_construction_fixes_user_actor() {
        let command = ReplaceSessionMetadata::new(command_id(1), session_id(2), metadata(false));

        assert_eq!(command.actor(), Actor::User);
    }

    /// tool agency is semantic replay payload rather than an
    /// interchangeable attribution side channel.
    #[test]
    fn command_equality_includes_tool_actor() {
        let user = ReplaceSessionMetadata::new(command_id(1), session_id(2), metadata(false));
        let tool = ReplaceSessionMetadata::new_for_tool(
            command_id(1),
            session_id(2),
            tool_request_id(3),
            metadata(false),
        );
        let same_tool_another_identity = ReplaceSessionMetadata::new_for_tool(
            command_id(5),
            session_id(2),
            tool_request_id(3),
            metadata(false),
        );
        let another_tool = ReplaceSessionMetadata::new_for_tool(
            command_id(1),
            session_id(2),
            tool_request_id(4),
            metadata(false),
        );

        assert_ne!(user, tool);
        assert_eq!(tool, same_tool_another_identity);
        assert_eq!(
            command_hash(&tool),
            command_hash(&same_tool_another_identity)
        );
        assert_ne!(tool, another_tool);
    }

    #[test]
    fn tool_metadata_command_retains_exact_request_agency() {
        let request = tool_request_id(3);
        let command = ReplaceSessionMetadata::new_for_tool(
            command_id(1),
            session_id(2),
            request,
            metadata(false),
        );

        assert_eq!(command.actor(), Actor::Tool { request });
    }

    #[test]
    fn tool_applied_result_carries_tool_last_writer() {
        let request = tool_request_id(3);
        let command = ReplaceSessionMetadata::new_for_tool(
            command_id(1),
            session_id(2),
            request,
            metadata(true),
        );
        let updated_at = SessionMetadataUpdatedAt::from_unix_micros(17);
        let prepared = command.prepare_applied(updated_at);
        let ReplaceSessionMetadataResult::Applied(applied) = prepared.result() else {
            panic!("fixture prepares an applied result")
        };

        assert_eq!(
            applied.snapshot().last_writer(),
            Some(SessionMetadataLastWriter::new(
                updated_at,
                Actor::Tool { request },
            ))
        );
    }

    #[test]
    fn prepared_applied_result_carries_command_actor_and_clock_evidence() {
        let command = ReplaceSessionMetadata::new(command_id(1), session_id(2), metadata(true));
        let updated_at = SessionMetadataUpdatedAt::from_unix_micros(17);
        let prepared = command.clone().prepare_applied(updated_at);
        let ReplaceSessionMetadataResult::Applied(applied) = prepared.result() else {
            panic!("fixture prepares an applied result");
        };

        assert_eq!(prepared.command(), &command);
        assert_eq!(applied.snapshot().session(), command.session());
        assert_eq!(applied.snapshot().content(), command.replacement());
        assert_eq!(
            applied.snapshot().last_writer(),
            Some(SessionMetadataLastWriter::new(updated_at, Actor::User))
        );
    }

    #[test]
    fn prepared_missing_session_rejection_retains_target() {
        let command = ReplaceSessionMetadata::new(command_id(1), session_id(2), metadata(false));
        let prepared = command.clone().prepare_session_not_found();
        let ReplaceSessionMetadataResult::Rejected(
            ReplaceSessionMetadataRejectedResult::SessionNotFound(rejected),
        ) = prepared.result()
        else {
            panic!("fixture prepares a missing-session rejection");
        };

        assert_eq!(prepared.command(), &command);
        assert_eq!(rejected.session(), command.session());
    }

    /// complete matching stored facts reconstruct the exact command
    /// and its recorded applied receipt without consulting current metadata.
    #[test]
    fn matching_applied_facts_reconstitute() {
        let command = ReplaceSessionMetadata::new(command_id(1), session_id(2), metadata(true));
        let updated_at = SessionMetadataUpdatedAt::from_unix_micros(17);
        let reconstructed = ReplaceSessionMetadataReconstitutionInput::applied(
            command.clone(),
            command.actor(),
            command.session(),
            updated_at,
            command.actor(),
        )
        .reconstitute()
        .expect("matching durable facts are complete");
        let ReplaceSessionMetadataResult::Applied(applied) = reconstructed.result() else {
            panic!("matching applied facts retain their result kind");
        };

        assert_eq!(reconstructed.command(), &command);
        assert_eq!(applied.snapshot().content(), command.replacement());
        assert_eq!(
            applied.snapshot().last_writer(),
            Some(SessionMetadataLastWriter::new(updated_at, command.actor()))
        );
    }

    /// matching tool-attributed facts reconstruct the exact command
    /// and last-writer agency.
    #[test]
    fn matching_tool_applied_facts_reconstitute() {
        let request = tool_request_id(3);
        let command = ReplaceSessionMetadata::new_for_tool(
            command_id(1),
            session_id(2),
            request,
            metadata(true),
        );
        let updated_at = SessionMetadataUpdatedAt::from_unix_micros(17);
        let reconstructed = ReplaceSessionMetadataReconstitutionInput::applied(
            command.clone(),
            command.actor(),
            command.session(),
            updated_at,
            command.actor(),
        )
        .reconstitute()
        .expect("matching tool-attributed facts are complete");
        let ReplaceSessionMetadataResult::Applied(applied) = reconstructed.result() else {
            panic!("matching tool-attributed facts retain their result kind")
        };

        assert_eq!(reconstructed.command(), &command);
        assert_eq!(
            applied.snapshot().last_writer(),
            Some(SessionMetadataLastWriter::new(
                updated_at,
                Actor::Tool { request },
            ))
        );
    }

    /// a recorded user command cannot substitute recovery agency.
    #[test]
    fn applied_reconstitution_rejects_recovery_for_user_command() {
        let command = ReplaceSessionMetadata::new(command_id(1), session_id(2), metadata(false));
        let input = ReplaceSessionMetadataReconstitutionInput::applied(
            command.clone(),
            Actor::Recovery,
            command.session(),
            SessionMetadataUpdatedAt::from_unix_micros(17),
            command.actor(),
        );
        let error = input
            .clone()
            .reconstitute()
            .expect_err("recovery cannot replace the canonical user actor");

        assert_eq!(
            error.failure(),
            ReplaceSessionMetadataReconstitutionFailure::CommandActorMismatch
        );
        assert_eq!(error.input().command(), &command);
        assert_eq!(error.into_parts().0.command(), &command);
    }

    /// a rejected user command cannot substitute recovery agency.
    #[test]
    fn rejected_reconstitution_rejects_recovery_for_user_command() {
        let command = ReplaceSessionMetadata::new(command_id(1), session_id(2), metadata(false));
        let error = ReplaceSessionMetadataReconstitutionInput::rejected_session_not_found(
            command.clone(),
            Actor::Recovery,
            command.session(),
        )
        .reconstitute()
        .expect_err("recovery cannot replace the canonical user actor");

        assert_eq!(
            error.failure(),
            ReplaceSessionMetadataReconstitutionFailure::CommandActorMismatch
        );
        assert_eq!(error.input().command(), &command);
    }

    /// an applied result cannot attribute a different writer from the
    /// canonical command.
    #[test]
    fn reconstitution_rejects_cross_wired_actor() {
        let command = ReplaceSessionMetadata::new(command_id(1), session_id(2), metadata(false));
        let input = ReplaceSessionMetadataReconstitutionInput::applied(
            command.clone(),
            command.actor(),
            command.session(),
            SessionMetadataUpdatedAt::from_unix_micros(17),
            Actor::Recovery,
        );
        let error = input
            .clone()
            .reconstitute()
            .expect_err("a different result actor fails closed");

        assert_eq!(
            error.failure(),
            ReplaceSessionMetadataReconstitutionFailure::ResultActorMismatch
        );
        assert_eq!(error.input().command(), &command);
        assert_eq!(error.into_parts().0.command(), &command);
    }

    /// an applied result cannot name another session.
    #[test]
    fn applied_reconstitution_rejects_cross_wired_session() {
        let command = ReplaceSessionMetadata::new(command_id(1), session_id(2), metadata(false));
        let error = ReplaceSessionMetadataReconstitutionInput::applied(
            command.clone(),
            command.actor(),
            session_id(3),
            SessionMetadataUpdatedAt::from_unix_micros(17),
            command.actor(),
        )
        .reconstitute()
        .expect_err("a different applied-result session fails closed");

        assert_eq!(
            error.failure(),
            ReplaceSessionMetadataReconstitutionFailure::ResultSessionMismatch
        );
        assert_eq!(error.input().command(), &command);
    }

    /// a terminal result cannot name another session.
    #[test]
    fn rejected_reconstitution_rejects_cross_wired_session() {
        let command = ReplaceSessionMetadata::new(command_id(1), session_id(2), metadata(false));
        let error = ReplaceSessionMetadataReconstitutionInput::rejected_session_not_found(
            command.clone(),
            command.actor(),
            session_id(3),
        )
        .reconstitute()
        .expect_err("a different result session fails closed");

        assert_eq!(
            error.failure(),
            ReplaceSessionMetadataReconstitutionFailure::ResultSessionMismatch
        );
        assert_eq!(error.input().command(), &command);
    }
}

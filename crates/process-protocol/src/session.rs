//! Session wire representations and validation.

use crate::scalars::{
    CanonicalU64, CanonicalUuid, CanonicalValueError, FrameValidationError,
    MAX_SESSION_METADATA_INDEXED_UTF8_BYTES, MAX_SESSION_METADATA_TOTAL_UTF8_BYTES,
    deserialize_required_nullable,
};
use crate::shared_validation::validate_imported_display_title;
use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// Explicit acknowledgement carried by a root-placement creation or update.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RootPlacementGlobalReadIntent {
    /// The caller explicitly accepts that root placement grants global read.
    Acknowledged,
}

/// One session's opt-in dotted placement decision.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionPlacement {
    /// Preserve legacy unrestricted conversation-read behavior.
    Pathless {},
    /// Place below root; the parent directory's subtree is readable.
    Scoped { path: String },
    /// Place at root with loud acknowledgement that this grants global read.
    RootGlobalRead {
        path: String,
        intent: RootPlacementGlobalReadIntent,
    },
}

impl SessionPlacement {
    pub(crate) fn is_pathless(&self) -> bool {
        matches!(self, Self::Pathless {})
    }
    /// Constructs and validates a non-root placement.
    pub fn try_scoped(path: String) -> Result<Self, CanonicalValueError> {
        let placement = Self::Scoped { path };
        validate_session_placement_shape(&placement).map_err(|_| CanonicalValueError::Placement)?;
        Ok(placement)
    }

    /// Constructs and validates the loud root-global-read decision.
    pub fn try_root_global_read(path: String) -> Result<Self, CanonicalValueError> {
        let placement = Self::RootGlobalRead {
            path,
            intent: RootPlacementGlobalReadIntent::Acknowledged,
        };
        validate_session_placement_shape(&placement).map_err(|_| CanonicalValueError::Placement)?;
        Ok(placement)
    }
}

impl Default for SessionPlacement {
    fn default() -> Self {
        Self::Pathless {}
    }
}

pub(crate) fn validate_session_placement_shape(
    placement: &SessionPlacement,
) -> Result<(), FrameValidationError> {
    let (path, root) = match placement {
        SessionPlacement::Pathless {} => return Ok(()),
        SessionPlacement::Scoped { path } => (path, false),
        SessionPlacement::RootGlobalRead { path, .. } => (path, true),
    };
    if path.len() > 64 * 64 + 63 {
        return Err(FrameValidationError::PlacementShape);
    }
    let segment_count = path.split('.').try_fold(0_usize, |count, segment| {
        let next_count = count + 1;
        (next_count <= 64
            && !segment.is_empty()
            && segment.len() <= 64
            && segment
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')))
        .then_some(next_count)
    });
    let shape_valid = segment_count.is_some_and(|count| (count == 1) == root);
    if shape_valid {
        Ok(())
    } else {
        Err(FrameValidationError::PlacementShape)
    }
}

/// One exact complete session-metadata object.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionMetadata {
    title: Option<String>,
    tags: Vec<String>,
    attributes: MetadataAttributes,
    archived: bool,
}

impl SessionMetadata {
    /// Validates and canonicalizes one complete metadata object.
    pub fn try_new(
        title: Option<String>,
        tags: Vec<String>,
        attributes: Vec<(String, String)>,
        archived: bool,
    ) -> Result<Self, CanonicalValueError> {
        Self::try_new_with_count_limits(title, tags, attributes, archived, None, None)
    }

    /// Validates canonical metadata and deployment tag and attribute policies.
    pub fn try_new_with_count_limits(
        title: Option<String>,
        tags: Vec<String>,
        attributes: Vec<(String, String)>,
        archived: bool,
        max_tags: Option<usize>,
        max_attributes: Option<usize>,
    ) -> Result<Self, CanonicalValueError> {
        let mut total_utf8_bytes = 0usize;
        if let Some(title) = title.as_deref() {
            validate_nonempty_metadata_text(title)?;
            add_metadata_utf8_bytes(&mut total_utf8_bytes, title)?;
        }
        let tags = canonical_metadata_tags(tags, max_tags)?;
        for tag in &tags {
            add_metadata_utf8_bytes(&mut total_utf8_bytes, tag)?;
        }
        let attributes = MetadataAttributes::try_new(attributes, max_attributes)?;
        for (key, value) in &attributes.0 {
            add_metadata_utf8_bytes(&mut total_utf8_bytes, key)?;
            add_metadata_utf8_bytes(&mut total_utf8_bytes, value)?;
        }
        Ok(Self {
            title,
            tags,
            attributes,
            archived,
        })
    }

    /// Constructs the unwritten empty, non-archived object.
    pub fn empty() -> Self {
        Self {
            title: None,
            tags: Vec::new(),
            attributes: MetadataAttributes(BTreeMap::new()),
            archived: false,
        }
    }

    /// Borrows the optional exact title.
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// Iterates tags in exact scalar order.
    pub fn tags(&self) -> impl ExactSizeIterator<Item = &str> {
        self.tags.iter().map(String::as_str)
    }

    /// Iterates attributes in exact key scalar order.
    pub fn attributes(&self) -> impl ExactSizeIterator<Item = (&str, &str)> {
        self.attributes
            .0
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
    }

    /// Returns whether the session is archived.
    pub const fn archived(&self) -> bool {
        self.archived
    }

    pub(crate) fn is_initial(&self) -> bool {
        self.title.is_none()
            && self.tags.is_empty()
            && self.attributes.0.is_empty()
            && !self.archived
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSessionMetadata {
    #[serde(deserialize_with = "deserialize_required_nullable")]
    title: Option<String>,
    #[serde(deserialize_with = "deserialize_session_metadata_tags")]
    tags: Vec<String>,
    attributes: MetadataAttributes,
    archived: bool,
}

impl<'de> Deserialize<'de> for SessionMetadata {
    fn deserialize<DeserializerT>(deserializer: DeserializerT) -> Result<Self, DeserializerT::Error>
    where
        DeserializerT: Deserializer<'de>,
    {
        let raw = RawSessionMetadata::deserialize(deserializer)?;
        Self::try_new(
            raw.title,
            raw.tags,
            raw.attributes.0.into_iter().collect(),
            raw.archived,
        )
        .map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
struct MetadataAttributes(BTreeMap<String, String>);

impl MetadataAttributes {
    fn try_new(
        values: Vec<(String, String)>,
        maximum: Option<usize>,
    ) -> Result<Self, CanonicalValueError> {
        if maximum.is_some_and(|maximum| values.len() > maximum) {
            return Err(CanonicalValueError::Metadata);
        }
        let mut attributes = BTreeMap::new();
        for (key, value) in values {
            validate_nonempty_metadata_text(&key)?;
            validate_indexed_metadata_text(&key)?;
            validate_metadata_text(&value)?;
            if attributes.insert(key, value).is_some() {
                return Err(CanonicalValueError::Metadata);
            }
        }
        Ok(Self(attributes))
    }
}

struct MetadataAttributesVisitor;

impl<'de> Visitor<'de> for MetadataAttributesVisitor {
    type Value = MetadataAttributes;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an exact session metadata attribute object")
    }

    fn visit_map<AccessT>(self, mut map: AccessT) -> Result<Self::Value, AccessT::Error>
    where
        AccessT: MapAccess<'de>,
    {
        let mut attributes = BTreeMap::new();
        while let Some((key, value)) = map.next_entry::<String, String>()? {
            validate_nonempty_metadata_text(&key).map_err(serde::de::Error::custom)?;
            validate_indexed_metadata_text(&key).map_err(serde::de::Error::custom)?;
            validate_metadata_text(&value).map_err(serde::de::Error::custom)?;
            if attributes.insert(key, value).is_some() {
                return Err(serde::de::Error::custom(
                    "duplicate session metadata attribute key",
                ));
            }
        }
        Ok(MetadataAttributes(attributes))
    }
}

impl<'de> Deserialize<'de> for MetadataAttributes {
    fn deserialize<DeserializerT>(deserializer: DeserializerT) -> Result<Self, DeserializerT::Error>
    where
        DeserializerT: Deserializer<'de>,
    {
        deserializer.deserialize_map(MetadataAttributesVisitor)
    }
}

fn validate_metadata_text(value: &str) -> Result<(), CanonicalValueError> {
    if value.contains('\0') {
        Err(CanonicalValueError::Metadata)
    } else {
        Ok(())
    }
}

pub(crate) fn validate_nonempty_metadata_text(value: &str) -> Result<(), CanonicalValueError> {
    if value.is_empty() {
        Err(CanonicalValueError::Metadata)
    } else {
        validate_metadata_text(value)
    }
}

fn validate_indexed_metadata_text(value: &str) -> Result<(), CanonicalValueError> {
    if value.len() > MAX_SESSION_METADATA_INDEXED_UTF8_BYTES {
        Err(CanonicalValueError::Metadata)
    } else {
        Ok(())
    }
}

pub(crate) fn add_metadata_utf8_bytes(
    total: &mut usize,
    value: &str,
) -> Result<(), CanonicalValueError> {
    *total = total.saturating_add(value.len());
    if *total > MAX_SESSION_METADATA_TOTAL_UTF8_BYTES {
        Err(CanonicalValueError::Metadata)
    } else {
        Ok(())
    }
}

pub(crate) fn canonical_metadata_tags(
    values: Vec<String>,
    maximum: Option<usize>,
) -> Result<Vec<String>, CanonicalValueError> {
    if maximum.is_some_and(|maximum| values.len() > maximum) {
        return Err(CanonicalValueError::Metadata);
    }
    let mut tags = BTreeSet::new();
    for tag in values {
        validate_nonempty_metadata_text(&tag)?;
        validate_indexed_metadata_text(&tag)?;
        if !tags.insert(tag) {
            return Err(CanonicalValueError::Metadata);
        }
    }
    Ok(tags.into_iter().collect())
}

pub(crate) fn deserialize_session_metadata_tags<'de, DeserializerT>(
    deserializer: DeserializerT,
) -> Result<Vec<String>, DeserializerT::Error>
where
    DeserializerT: Deserializer<'de>,
{
    Vec::<String>::deserialize(deserializer)
}

pub(crate) fn deserialize_required_metadata_tags<'de, DeserializerT>(
    deserializer: DeserializerT,
) -> Result<Vec<String>, DeserializerT::Error>
where
    DeserializerT: Deserializer<'de>,
{
    Vec::<String>::deserialize(deserializer)
}

/// Closed actor provenance carried by a metadata last-writer stamp.
///
/// The variants mirror the domain actor inventory exactly, because durable
/// metadata already records every one of them: the tool-facing replacement
/// constructor stamps a tool writer, and a narrower wire enum would leave a
/// readable durable snapshot with no wire projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum MetadataActor {
    /// The user wrote the snapshot.
    User {},
    /// Daemon core wrote the snapshot without delegated agency.
    Core {},
    /// Model output from one exact turn wrote the snapshot.
    Model {
        /// The turn whose model output acted.
        turn_id: CanonicalUuid,
    },
    /// The startup recovery scan wrote the snapshot.
    Recovery {},
    /// Execution of one exact tool request wrote the snapshot.
    Tool {
        /// The tool request whose execution acted.
        tool_request_id: CanonicalUuid,
    },
}

#[derive(signalbox_derive::Accessors)]
/// The post-lock database statement time and actor of the latest replacement.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetadataLastWriter {
    /// Returns the nonnegative Unix-microsecond transaction timestamp.
    #[get(copy)]
    updated_at_unix_micros: CanonicalU64,
    /// Returns the closed actor provenance.
    #[get(copy)]
    actor: MetadataActor,
}

/// How a new live session relates to one selected imported frontier.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportedSessionRelationship {
    /// Continue from the selected imported boundary.
    Resume,
    /// Branch from the selected imported boundary.
    Fork,
}

/// One closed client-selected treatment for submitted input.
///
/// Omitting this value from `submit_input` preserves the baseline
/// start-when-idle treatment. Steering and queueing carry the exact active turn
/// the client observed so the domain can reject a stale target.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum InputDelivery {
    /// Start new work only while the session slot is idle.
    StartWhenIdle {},
    /// Bind the input to the active turn's next safe point.
    Steer {
        /// Exact active turn observed by the client.
        expected_active_turn_id: CanonicalUuid,
    },
    /// Queue new work behind the active turn.
    Queue {
        /// Exact active turn observed by the client.
        expected_active_turn_id: CanonicalUuid,
    },
}

pub(crate) fn deserialize_present_input_delivery<'de, DeserializerT>(
    deserializer: DeserializerT,
) -> Result<Option<InputDelivery>, DeserializerT::Error>
where
    DeserializerT: Deserializer<'de>,
{
    InputDelivery::deserialize(deserializer).map(Some)
}

impl MetadataLastWriter {
    /// Constructs one exact last-writer stamp.
    pub const fn new(updated_at_unix_micros: CanonicalU64, actor: MetadataActor) -> Self {
        Self {
            updated_at_unix_micros,
            actor,
        }
    }
}

/// One closed conversation origin class.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationOrigin {
    /// A native session.
    NativeSession,
    /// An immutable imported conversation.
    ImportedConversation,
}

/// Which conversation origin classes one unified list request selects.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationOriginFilter {
    /// Native sessions only.
    Native,
    /// Imported conversations only.
    Imported,
    /// Both origin classes.
    All,
}

#[derive(signalbox_derive::Accessors)]
/// One exclusive unified keyset cursor naming the last listed conversation.
///
/// The unified page order is by conversation identity UUID value, native
/// before imported for a theoretical equal identity, so the cursor names one
/// total position across both origin classes.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationCursor {
    /// Returns the origin class of the cursor position.
    #[get(copy)]
    /// Origin class of the cursor position.
    origin: ConversationOrigin,
    /// Returns the conversation identity at the cursor position.
    #[get(copy)]
    /// Conversation identity at the cursor position.
    conversation_id: CanonicalUuid,
}

impl ConversationCursor {
    /// Names one exact unified cursor position.
    pub const fn new(origin: ConversationOrigin, conversation_id: CanonicalUuid) -> Self {
        Self {
            origin,
            conversation_id,
        }
    }
}

/// One exact stored imported source format and converter version.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportedConversationSourceFormat {
    /// Claude Code session JSONL interpreted by converter version 1.
    ClaudeCodeSessionJsonlV1,
    /// Claude Code session JSONL interpreted by converter version 2.
    ClaudeCodeSessionJsonlV2,
    /// Codex rollout JSONL interpreted by converter version 1.
    CodexRolloutJsonlV1,
}

/// One closed per-origin unified conversation summary.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "origin", rename_all = "snake_case", deny_unknown_fields)]
pub enum ConversationSummary {
    /// One native session with its current organizational facts.
    NativeSession {
        /// Session identity.
        session_id: CanonicalUuid,
        /// Exact optional metadata title.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        title: Option<String>,
        /// Whether the session is archived.
        archived: bool,
        /// Current defaults version.
        defaults_version: CanonicalU64,
    },
    /// One immutable imported conversation snapshot.
    ImportedConversation {
        /// Imported conversation identity.
        imported_conversation_id: CanonicalUuid,
        /// Exact optional source-derived display title.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        title: Option<String>,
        /// Total normalized entry count; the greatest position a
        /// continuation may select.
        entry_count: CanonicalU64,
        /// Exact stored source format and converter version.
        source_format: ImportedConversationSourceFormat,
    },
}

impl ConversationSummary {
    /// Returns the unified cursor position this summary occupies.
    pub const fn cursor(&self) -> ConversationCursor {
        match self {
            Self::NativeSession { session_id, .. } => {
                ConversationCursor::new(ConversationOrigin::NativeSession, *session_id)
            }
            Self::ImportedConversation {
                imported_conversation_id,
                ..
            } => ConversationCursor::new(
                ConversationOrigin::ImportedConversation,
                *imported_conversation_id,
            ),
        }
    }

    pub(crate) fn validate(&self) -> Result<(), FrameValidationError> {
        match self {
            Self::NativeSession {
                title,
                defaults_version,
                ..
            } => {
                if let Some(title) = title {
                    validate_nonempty_metadata_text(title)
                        .map_err(|_| FrameValidationError::ConversationListShape)?;
                    let mut total_utf8_bytes = 0usize;
                    add_metadata_utf8_bytes(&mut total_utf8_bytes, title)
                        .map_err(|_| FrameValidationError::ConversationListShape)?;
                }
                if defaults_version.value() == 0 {
                    return Err(FrameValidationError::ConversationListShape);
                }
                Ok(())
            }
            Self::ImportedConversation {
                title, entry_count, ..
            } => {
                if let Some(title) = title {
                    validate_imported_display_title(title)?;
                }
                if entry_count.value() == 0 {
                    return Err(FrameValidationError::ConversationListShape);
                }
                Ok(())
            }
        }
    }
}

use super::*;

/// One imported entry as the imported verb presents it.
pub(crate) struct ImportedEntryRow<'a> {
    pub(crate) position: u64,
    pub(crate) imported_entry_id: CanonicalUuid,
    pub(crate) source_speaker: ImportedSourceSpeaker,
    pub(crate) content_kind: ImportedContentKind,
    pub(crate) text_preview: Option<&'a ImportedTextPreview>,
}

/// One complete metadata summary as the search verb presents it.
pub(crate) struct SessionMetadataRow<'a> {
    pub(crate) session_id: CanonicalUuid,
    pub(crate) defaults_version: u64,
    pub(crate) selection: &'a str,
    pub(crate) dangerous_tool_auto_approval: bool,
    pub(crate) archived: bool,
    pub(crate) last_writer: Option<MetadataLastWriter>,
    pub(crate) tags: &'a [String],
    pub(crate) title: Option<&'a str>,
}

/// One unified conversation summary as the conversations verb presents it.
pub(crate) enum ConversationRow<'a> {
    /// One native session line.
    Native {
        session_id: CanonicalUuid,
        archived: bool,
        defaults_version: u64,
        title: Option<&'a str>,
    },
    /// One imported conversation line; the entry count is the greatest
    /// `--through-position` a continuation may select.
    Imported {
        imported_conversation_id: CanonicalUuid,
        format: &'static str,
        entry_count: u64,
        title: Option<&'a str>,
    },
}

/// What one process-derived text field may carry unescaped, given where it
/// sits in the output that carries it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TextField {
    /// Flowing text that owns the lines it is written to, so U+000A is its
    /// content rather than a delimiter.
    Flowing,
    /// The last named value on its line: a line feed inside it would forge a
    /// following line, and nothing else delimits it.
    TrailingOnLine,
    /// A value delimited within its line: the space that ends its field and
    /// the comma that separates it from a sibling are escaped too, so the
    /// field states its exact values.
    DelimitedOnLine,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ChatTurnStatus {
    Queued(CanonicalUuid),
    Active(CanonicalUuid),
    AwaitingApproval {
        turn_id: CanonicalUuid,
        tool_request_id: CanonicalUuid,
    },
}

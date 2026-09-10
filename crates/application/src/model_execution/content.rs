use super::{
    AcceptedInputId, AssistantText, AttachmentKind, BlobDigest, ContextCompactionRange,
    DelegationContent, DelegationMessageId, DelegationOutcome, DirectModelSelection, ImportedText,
    ImportedTranscriptEntryId, ModelCallId, NonZeroU64, ProviderCompactionBlock,
    SemanticTranscriptEntryRef, SessionConfigurationDefaultsVersion, SessionId, ToolDenialReason,
    ToolExecutionError, ToolRequest, ToolRequestId, ToolResultContent, TurnId, UserContent,
    UserContentPart, fmt,
};

/// Non-secret durable name of the credential pinned for one model call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelCallCredentialReference(String);

impl ModelCallCredentialReference {
    /// Preserves the deployment-owned reference spelling exactly.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrows the non-secret reference text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One provider-neutral text part derived from ordered accepted-input content.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelUserContentPart {
    /// Exact user-authored text.
    Text(signalbox_domain::NonEmptyUnicodeText),
    /// Canonical compact-JSON attachment stub; never attachment bytes.
    AttachmentStub(ModelAttachmentStub),
}

impl ModelUserContentPart {
    /// Borrows the exact provider-visible text for this part.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Text(value) => value.as_str(),
            Self::AttachmentStub(stub) => stub.as_str(),
        }
    }

    fn corresponds_to(&self, source: &UserContentPart) -> bool {
        match (self, source) {
            (Self::Text(rendered), UserContentPart::Text { value }) => rendered == value,
            (
                Self::AttachmentStub(rendered),
                UserContentPart::Attachment {
                    digest,
                    kind,
                    media_type,
                    display_filename,
                },
            ) => {
                rendered.digest == *digest
                    && rendered.kind == *kind
                    && &rendered.media_type == media_type
                    && &rendered.display_filename == display_filename
            }
            (Self::Text(_), UserContentPart::Attachment { .. })
            | (Self::AttachmentStub(_), UserContentPart::Text { .. }) => false,
        }
    }
}

/// Provider-neutral ordered text projection of one accepted input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelUserContent {
    pub(super) parts: Box<[ModelUserContentPart]>,
}

impl ModelUserContent {
    /// Borrows the ordered provider-visible text and attachment-stub parts.
    pub fn parts(&self) -> &[ModelUserContentPart] {
        &self.parts
    }

    /// Borrows text when the source content is exactly one text part.
    pub fn single_text(&self) -> Option<&signalbox_domain::NonEmptyUnicodeText> {
        match self.parts.as_ref() {
            [ModelUserContentPart::Text(value)] => Some(value),
            _ => None,
        }
    }
}

impl PartialEq<UserContent> for ModelUserContent {
    fn eq(&self, other: &UserContent) -> bool {
        self.parts.len() == other.parts().len()
            && self
                .parts
                .iter()
                .zip(other.parts())
                .all(|(rendered, source)| rendered.corresponds_to(source))
    }
}

/// Canonical bounded model-visible metadata for one attachment.
#[derive(Clone, Eq, PartialEq)]
pub struct ModelAttachmentStub {
    pub(super) rendered: String,
    pub(super) digest: BlobDigest,
    pub(super) kind: AttachmentKind,
    pub(super) media_type: signalbox_domain::DeclaredMediaType,
    pub(super) display_filename: Option<signalbox_domain::AttachmentDisplayFilename>,
}

/// Identity of one attachment occurrence in a rendered semantic entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RenderedAttachmentSelector {
    entry: signalbox_domain::SemanticTranscriptEntryId,
    part_ordinal: u8,
}

impl RenderedAttachmentSelector {
    /// Selects a zero-based part within one globally identified semantic entry.
    pub const fn new(entry: signalbox_domain::SemanticTranscriptEntryId, part_ordinal: u8) -> Self {
        Self {
            entry,
            part_ordinal,
        }
    }

    /// Returns the semantic entry identity.
    pub const fn entry(self) -> signalbox_domain::SemanticTranscriptEntryId {
        self.entry
    }

    /// Returns the zero-based ordinal, including intervening text parts.
    pub const fn part_ordinal(self) -> u8 {
        self.part_ordinal
    }

    /// Parses only the canonical model-visible spelling.
    pub fn parse(value: &str) -> Option<Self> {
        let (entry, ordinal) = value.split_once('_')?;
        let selector = Self::new(
            signalbox_domain::SemanticTranscriptEntryId::from_uuid(
                uuid::Uuid::parse_str(entry).ok()?,
            ),
            ordinal.parse().ok()?,
        );
        (selector.to_string() == value).then_some(selector)
    }
}

impl fmt::Display for RenderedAttachmentSelector {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}_{}",
            self.entry.into_uuid().simple(),
            self.part_ordinal
        )
    }
}

impl ModelAttachmentStub {
    /// Borrows the exact compact JSON spelling.
    pub fn as_str(&self) -> &str {
        &self.rendered
    }
}

impl fmt::Debug for ModelAttachmentStub {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ModelAttachmentStub(<redacted>)")
    }
}

/// Durable identity and credential facts for one retained reasoning item.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderReasoningProvenance {
    /// Source-qualified semantic entry carrying the item.
    pub source: SemanticTranscriptEntryRef,
    /// Outcome-authoritative call that produced the item.
    pub producing_call: ModelCallId,
    /// Effective serving target pinned on the producing call.
    pub producing_target: signalbox_domain::ResolvedProviderTarget,
    /// Non-secret credential reference pinned on the producing call.
    pub producing_credential: ModelCallCredentialReference,
}

/// Application rendering of one semantic frontier entry as a provider message.
///
/// The source-qualified semantic entry, rather than a native turn assumption,
/// preserves the provenance of entries inherited across sessions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelConversationMessage {
    /// Injected session event resolved from the exact successor placement record.
    RunnerPlacementChanged {
        /// Source-qualified placement boundary.
        source: SemanticTranscriptEntryRef,
        /// Positive successor placement revision.
        placement_revision: signalbox_domain::RunnerGeneration,
        /// Sandbox selected by the referenced placement record.
        sandbox: signalbox_domain::RunnerSandboxProfile,
    },
    /// Injected session event declaring the model identity newly in force.
    ModelIdentityChanged {
        /// The source-qualified semantic entry being rendered.
        source: SemanticTranscriptEntryRef,
        /// The immutable defaults epoch bound by the starting turn.
        defaults_version: SessionConfigurationDefaultsVersion,
        /// The exact direct model identity newly selected.
        selected: DirectModelSelection,
    },
    /// Model-produced summary standing in for one exact earlier range.
    ContextSummary {
        /// The source-qualified summary entry being rendered.
        source: SemanticTranscriptEntryRef,
        /// The dedicated model call that produced the summary.
        producing_call: ModelCallId,
        /// The exact inclusive range represented by this summary.
        summarized: ContextCompactionRange,
        /// Exact model-produced summary text.
        content: AssistantText,
    },
    /// Exact accepted-input origin content rendered with the user role.
    User {
        /// The source-qualified semantic entry being rendered.
        source: SemanticTranscriptEntryRef,
        /// The immutable accepted input carrying this content.
        accepted_input: AcceptedInputId,
        /// Ordered provider-neutral text and attachment-stub parts.
        content: ModelUserContent,
    },
    /// Model-authored task injected into one delegated child's first turn.
    DelegatedTask {
        source: SemanticTranscriptEntryRef,
        spawning_request: ToolRequestId,
        parent_session: SessionId,
        parent_turn: TurnId,
        content: DelegationContent,
    },
    /// Immutable peer content injected into the exact recipient session.
    DelegationMessage {
        source: SemanticTranscriptEntryRef,
        spawning_request: ToolRequestId,
        message: DelegationMessageId,
        sender: SessionId,
        recipient: SessionId,
        delivery_sequence: NonZeroU64,
        content: DelegationContent,
    },
    /// Background child completion injected as a session event, not a tool result.
    BackgroundDelegationResult {
        source: SemanticTranscriptEntryRef,
        awaiting_request: ToolRequestId,
        spawning_request: ToolRequestId,
        child: SessionId,
        delivery_sequence: NonZeroU64,
        outcome: DelegationOutcome,
    },
    /// Exact assistant content rendered with the assistant role.
    Assistant {
        /// The source-qualified semantic entry being rendered.
        source: SemanticTranscriptEntryRef,
        /// The outcome-authoritative call that produced the content.
        producing_call: ModelCallId,
        /// Exact assistant-owned text.
        content: AssistantText,
    },
    /// One complete provider reasoning item rendered for replay.
    ProviderReasoning {
        /// The source-qualified semantic entry.
        source: SemanticTranscriptEntryRef,
        /// The outcome-authoritative producing call.
        producing_call: ModelCallId,
        /// The complete retained provider item.
        item: signalbox_domain::ProviderReasoningItem,
    },
    /// One opaque provider-produced compaction block rendered with the assistant role.
    ProviderCompaction {
        /// The source-qualified semantic entry being rendered.
        source: SemanticTranscriptEntryRef,
        /// The outcome-authoritative call that produced the block.
        producing_call: ModelCallId,
        /// The complete provider block retained for exact replay.
        block: ProviderCompactionBlock,
    },
    /// One durable assistant tool proposal.
    AssistantToolUse {
        /// The source-qualified semantic entry being rendered.
        source: SemanticTranscriptEntryRef,
        /// The outcome-authoritative call that proposed the request.
        producing_call: ModelCallId,
        /// Immutable request content and hub correlation.
        request: ToolRequest,
    },
    /// One durable result corresponding to an earlier assistant proposal.
    ToolResult {
        /// The source-qualified semantic entry being rendered.
        source: SemanticTranscriptEntryRef,
        /// The logical request whose provider-visible correlation this resolves.
        request: ToolRequestId,
        /// Exact durable result classification and content.
        content: ModelToolResultContent,
    },
    /// Exact imported text rendered with its source-attested user role.
    ImportedUser {
        /// The source-qualified semantic projection entry being rendered.
        source: SemanticTranscriptEntryRef,
        /// The immutable imported entry that remains content authority.
        imported_entry: ImportedTranscriptEntryId,
        /// Exact decoded imported text, including empty text.
        content: ImportedText,
    },
    /// Exact imported text rendered with its source-attested assistant role.
    ImportedAssistant {
        /// The source-qualified semantic projection entry being rendered.
        source: SemanticTranscriptEntryRef,
        /// The immutable imported entry that remains content authority.
        imported_entry: ImportedTranscriptEntryId,
        /// Exact decoded imported text, including empty text.
        content: ImportedText,
    },
}

/// Provider-neutral result content resolved from durable request/attempt facts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelToolResultContent {
    /// Exact admitted executor success content.
    Success(ToolResultContent),
    /// Exact terminal executor error evidence.
    ExecutionError(ToolExecutionError),
    /// Exact durable user denial.
    Denied {
        /// Optional bounded sanitized user explanation.
        reason: Option<ToolDenialReason>,
    },
    /// The turn ended before this request received a decision.
    ClosedByTurnEnd,
    /// Exact typed terminal child outcome delivered to `await_session`.
    Delegation(DelegationOutcome),
}

#[derive(serde::Serialize)]
pub(super) struct SerializedAttachmentEnvelope<'a> {
    pub(super) signalbox_attachment: SerializedAttachmentStub<'a>,
}

#[derive(serde::Serialize)]
pub(super) struct SerializedAttachmentStub<'a> {
    pub(super) kind: &'static str,
    pub(super) media_type: &'a str,
    pub(super) display_filename: Option<&'a str>,
    pub(super) byte_length: String,
    pub(super) digest: String,
    pub(super) visible_part: String,
}

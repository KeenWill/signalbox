//! Daemon adapter from application conversation reads to model-facing tools.

use std::{error::Error, fmt, num::NonZeroU64};

use signalbox_application::{
    ClassifyOperatorFailure, ConversationListCursor as ApplicationCursor,
    ConversationListItem as ApplicationListItem, ConversationListQuery, ConversationOriginFilter,
    ConversationPageReader, ListConversationsService, OperatorFailureClass,
};
use signalbox_domain::{
    ImportedSourceAttestation, ImportedSpeaker, ImportedTranscriptContent,
    ImportedTranscriptEntryInput,
};
use signalbox_persistence::{
    conversation_import::{
        ImportedConversationRepositoryError, ImportedRawBlobStorageError, load_normalized_entries,
    },
    conversation_listing::{ConversationListingRepository, ConversationListingRepositoryError},
    process_read::{
        ProcessImportedContentKind, ProcessImportedSourceSpeaker, ProcessReadError,
        ProcessReadRepository, ProcessScopedTranscriptRead, ProcessTranscriptEntry,
        ProcessTranscriptItem,
    },
};
use signalbox_tools_conversations::{
    ConversationCursor, ConversationIntrospectionPort, ConversationListItem, ConversationListPage,
    ConversationListRequest, ConversationTranscriptRead, ConversationTranscriptRequest,
    ImportedTranscriptRequest, TranscriptEntry, TranscriptEntryKind, TranscriptPage,
};
use sqlx::PgPool;

/// PostgreSQL-backed application adapter for bounded conversation tools.
#[derive(Clone, Debug)]
pub struct PostgresConversationIntrospection {
    pool: PgPool,
}

impl PostgresConversationIntrospection {
    /// Composes the adapter from the daemon's already-injected pool.
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

/// Sanitized introspection adapter failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConversationIntrospectionError {
    class: OperatorFailureClass,
}

impl ConversationIntrospectionError {
    const fn caller_bug() -> Self {
        Self {
            class: OperatorFailureClass::CallerOrHubBug,
        }
    }

    const fn corrupt_projection() -> Self {
        Self {
            class: OperatorFailureClass::FailClosedCorruption,
        }
    }

    fn from_listing(error: ConversationListingRepositoryError) -> Self {
        Self {
            class: match error {
                ConversationListingRepositoryError::Database(_) => {
                    OperatorFailureClass::Infrastructure {
                        commit_ambiguous: false,
                    }
                }
                ConversationListingRepositoryError::Corruption(_) => {
                    OperatorFailureClass::FailClosedCorruption
                }
            },
        }
    }

    fn from_process(error: ProcessReadError) -> Self {
        Self {
            class: match error {
                ProcessReadError::Database(_) => OperatorFailureClass::Infrastructure {
                    commit_ambiguous: false,
                },
                ProcessReadError::Corruption(_) => OperatorFailureClass::FailClosedCorruption,
            },
        }
    }

    fn from_import(error: ImportedConversationRepositoryError) -> Self {
        Self {
            class: match error {
                ImportedConversationRepositoryError::Database(_) => {
                    OperatorFailureClass::Infrastructure {
                        commit_ambiguous: false,
                    }
                }
                ImportedConversationRepositoryError::IdentityCollision(_) => {
                    OperatorFailureClass::IdentityCollision
                }
                ImportedConversationRepositoryError::BlobStorage(
                    ImportedRawBlobStorageError::Unavailable,
                ) => OperatorFailureClass::Infrastructure {
                    commit_ambiguous: false,
                },
                ImportedConversationRepositoryError::BlobStorage(
                    ImportedRawBlobStorageError::Integrity,
                ) => OperatorFailureClass::FailClosedCorruption,
                ImportedConversationRepositoryError::BlobCatalog(_) => {
                    OperatorFailureClass::FailClosedCorruption
                }
                ImportedConversationRepositoryError::Corruption(_) => {
                    OperatorFailureClass::FailClosedCorruption
                }
            },
        }
    }
}

impl fmt::Display for ConversationIntrospectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("conversation introspection failed")
    }
}

impl Error for ConversationIntrospectionError {}

impl ClassifyOperatorFailure for ConversationIntrospectionError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        self.class
    }
}

impl ConversationIntrospectionPort for PostgresConversationIntrospection {
    type Error = ConversationIntrospectionError;

    async fn list_conversations(
        &mut self,
        request: ConversationListRequest,
    ) -> Result<ConversationListPage, Self::Error> {
        let after = request.after().map(application_cursor);
        let page_size = u64::try_from(request.max_results())
            .map_err(|_| ConversationIntrospectionError::caller_bug())?;
        let query = ConversationListQuery::try_new(
            None,
            ConversationOriginFilter::All,
            true,
            page_size,
            after,
        )
        .map_err(|_| ConversationIntrospectionError::caller_bug())?;
        let service =
            ListConversationsService::new(ConversationListingRepository::new(self.pool.clone()));
        let mut page = service
            .execute(query)
            .await
            .map_err(ConversationIntrospectionError::from_listing)?;
        let mut items = Vec::with_capacity(request.max_results());
        while let Some(item) = page
            .next_item()
            .await
            .map_err(ConversationIntrospectionError::from_listing)?
        {
            items.push(tool_list_item(item));
        }
        Ok(ConversationListPage::new(
            items,
            page.next_after().is_some(),
        ))
    }

    async fn read_conversation(
        &mut self,
        request: ConversationTranscriptRequest,
    ) -> Result<ConversationTranscriptRead, Self::Error> {
        let repository = ProcessReadRepository::new(self.pool.clone());
        let read = repository
            .open_scoped_transcript(request.requesting_session(), request.target_session())
            .await
            .map_err(ConversationIntrospectionError::from_process)?;
        let mut reader = match read {
            ProcessScopedTranscriptRead::Opened(reader) => reader,
            ProcessScopedTranscriptRead::TargetNotFound => {
                return Ok(ConversationTranscriptRead::NotFound);
            }
            ProcessScopedTranscriptRead::Refused(refusal) => {
                return Ok(ConversationTranscriptRead::Refused(refusal));
            }
        };
        let after = request.after_position().map_or(0, NonZeroU64::get);
        let mut builder = TranscriptPageBuilder::new(request.max_entries(), request.max_bytes());
        while let Some(item) = reader
            .next_item()
            .await
            .map_err(ConversationIntrospectionError::from_process)?
        {
            let ProcessTranscriptItem::Entry(entry) = item else {
                continue;
            };
            let visible = visible_process_entry(entry)?;
            if visible.position.get() <= after {
                continue;
            }
            if !builder.push(visible) {
                return Ok(ConversationTranscriptRead::Read(builder.finish(true)));
            }
        }
        Ok(ConversationTranscriptRead::Read(builder.finish(false)))
    }

    async fn read_imported_conversation(
        &mut self,
        request: ImportedTranscriptRequest,
    ) -> Result<Option<TranscriptPage>, Self::Error> {
        let Some(entries) = load_normalized_entries(&self.pool, request.conversation())
            .await
            .map_err(ConversationIntrospectionError::from_import)?
        else {
            return Ok(None);
        };
        let after = request.after_position().map_or(0, NonZeroU64::get);
        let mut builder = TranscriptPageBuilder::new(request.max_entries(), request.max_bytes());
        for entry in &entries {
            if entry.position().as_u64() <= after {
                continue;
            }
            if !builder.push(visible_imported_entry(entry)?) {
                return Ok(Some(builder.finish(true)));
            }
        }
        Ok(Some(builder.finish(false)))
    }
}

fn application_cursor(cursor: ConversationCursor) -> ApplicationCursor {
    match cursor {
        ConversationCursor::Native(session) => ApplicationCursor::NativeSession(session),
        ConversationCursor::Imported(conversation) => {
            ApplicationCursor::ImportedConversation(conversation)
        }
    }
}

fn tool_list_item(item: ApplicationListItem) -> ConversationListItem {
    match item {
        ApplicationListItem::NativeSession {
            session,
            title,
            archived,
            ..
        } => ConversationListItem::Native {
            session,
            title,
            archived,
        },
        ApplicationListItem::ImportedConversation {
            conversation,
            title,
            entry_count,
            ..
        } => ConversationListItem::Imported {
            conversation,
            title,
            entry_count,
        },
    }
}

struct VisibleEntry {
    position: NonZeroU64,
    kind: TranscriptEntryKind,
    content: String,
}

fn visible_process_entry(
    entry: ProcessTranscriptEntry,
) -> Result<VisibleEntry, ConversationIntrospectionError> {
    let (entry_index, kind, content) = match entry {
        ProcessTranscriptEntry::DelegatedTask {
            entry_index,
            content,
            ..
        }
        | ProcessTranscriptEntry::DelegationMessage {
            entry_index,
            content,
            ..
        } => (entry_index, TranscriptEntryKind::System, content),
        ProcessTranscriptEntry::DelegationResult {
            entry_index,
            content,
            ..
        } => (
            entry_index,
            TranscriptEntryKind::ToolResult,
            content.unwrap_or_else(|| String::from("delegated child returned no content")),
        ),
        ProcessTranscriptEntry::ModelIdentityChanged { entry_index, .. } => (
            entry_index,
            TranscriptEntryKind::System,
            String::from("model identity changed"),
        ),
        ProcessTranscriptEntry::ContextSummary {
            entry_index,
            content,
            ..
        } => (entry_index, TranscriptEntryKind::Assistant, content),
        ProcessTranscriptEntry::User {
            entry_index,
            content,
            ..
        } => (
            entry_index,
            TranscriptEntryKind::User,
            serde_json::to_string(&crate::process_runtime::wire_user_content(&content))
                .map_err(|_| ConversationIntrospectionError::corrupt_projection())?,
        ),
        ProcessTranscriptEntry::Assistant {
            entry_index,
            content,
            ..
        } => (entry_index, TranscriptEntryKind::Assistant, content),
        ProcessTranscriptEntry::ProviderCompaction { entry_index, .. } => (
            entry_index,
            TranscriptEntryKind::System,
            String::from("provider compaction"),
        ),
        ProcessTranscriptEntry::AssistantToolUse {
            entry_index,
            name,
            arguments,
            ..
        } => (
            entry_index,
            TranscriptEntryKind::ToolUse,
            format!("{name}\n{arguments}"),
        ),
        ProcessTranscriptEntry::ToolExecutionResult {
            entry_index,
            content,
            ..
        }
        | ProcessTranscriptEntry::ToolDenied {
            entry_index,
            content,
            ..
        }
        | ProcessTranscriptEntry::ToolClosed {
            entry_index,
            content,
            ..
        } => (entry_index, TranscriptEntryKind::ToolResult, content),
        ProcessTranscriptEntry::TurnFailed { entry_index, .. } => (
            entry_index,
            TranscriptEntryKind::System,
            String::from("turn failed"),
        ),
        ProcessTranscriptEntry::TurnCompleted { entry_index, .. } => (
            entry_index,
            TranscriptEntryKind::System,
            String::from("turn completed"),
        ),
        ProcessTranscriptEntry::TurnCancelled { entry_index, .. } => (
            entry_index,
            TranscriptEntryKind::System,
            String::from("turn cancelled"),
        ),
        ProcessTranscriptEntry::ImportedText {
            entry_index,
            source_speaker,
            content,
            ..
        } => (entry_index, process_imported_kind(source_speaker), content),
        ProcessTranscriptEntry::Imported {
            entry_index,
            content_kind,
            ..
        } => (
            entry_index,
            TranscriptEntryKind::System,
            process_imported_marker(content_kind),
        ),
    };
    let position = entry_index
        .checked_add(1)
        .and_then(NonZeroU64::new)
        .ok_or_else(ConversationIntrospectionError::caller_bug)?;
    Ok(VisibleEntry {
        position,
        kind,
        content,
    })
}

fn process_imported_kind(speaker: ProcessImportedSourceSpeaker) -> TranscriptEntryKind {
    match speaker {
        ProcessImportedSourceSpeaker::User => TranscriptEntryKind::User,
        ProcessImportedSourceSpeaker::Assistant => TranscriptEntryKind::Assistant,
        ProcessImportedSourceSpeaker::NotAttested
        | ProcessImportedSourceSpeaker::AttestedAbsent => TranscriptEntryKind::System,
    }
}

fn process_imported_marker(kind: ProcessImportedContentKind) -> String {
    String::from(match kind {
        ProcessImportedContentKind::SourceEvent => "imported source event",
        ProcessImportedContentKind::SourceMessageBlock => "imported message block",
        ProcessImportedContentKind::Text => "imported unattested text",
        ProcessImportedContentKind::ToolCall => "imported tool call",
        ProcessImportedContentKind::ToolResult => "imported tool result",
        ProcessImportedContentKind::Thinking => "imported thinking block",
        ProcessImportedContentKind::RedactedThinking => "imported redacted-thinking block",
        ProcessImportedContentKind::Document => "imported document block",
        ProcessImportedContentKind::MessageContentAbsent => "imported message content absent",
    })
}

fn visible_imported_entry(
    entry: &ImportedTranscriptEntryInput,
) -> Result<VisibleEntry, ConversationIntrospectionError> {
    let position = NonZeroU64::new(entry.position().as_u64())
        .ok_or_else(ConversationIntrospectionError::caller_bug)?;
    let kind = imported_content_kind(entry.source_speaker(), entry.content());
    let content = match entry.content() {
        ImportedTranscriptContent::Text(ImportedSourceAttestation::Attested(text)) => {
            text.as_str().to_owned()
        }
        ImportedTranscriptContent::SourceEvent { .. } => String::from("imported source event"),
        ImportedTranscriptContent::SourceMessageBlock { .. } => {
            String::from("imported message block")
        }
        ImportedTranscriptContent::Text(_) => String::from("imported unattested text"),
        ImportedTranscriptContent::ToolCall { .. } => String::from("imported tool call"),
        ImportedTranscriptContent::ToolResult { .. } => String::from("imported tool result"),
        ImportedTranscriptContent::Thinking { .. } => String::from("imported thinking block"),
        ImportedTranscriptContent::RedactedThinking { .. } => {
            String::from("imported redacted-thinking block")
        }
        ImportedTranscriptContent::Document { .. } => String::from("imported document block"),
        ImportedTranscriptContent::MessageContentAbsent(_) => {
            String::from("imported message content absent")
        }
    };
    Ok(VisibleEntry {
        position,
        kind,
        content,
    })
}

fn imported_content_kind(
    speaker: &ImportedSourceAttestation<ImportedSpeaker>,
    content: &ImportedTranscriptContent,
) -> TranscriptEntryKind {
    match content {
        ImportedTranscriptContent::Text(ImportedSourceAttestation::Attested(_)) => {
            imported_speaker_kind(speaker)
        }
        _ => TranscriptEntryKind::System,
    }
}

fn imported_speaker_kind(
    speaker: &ImportedSourceAttestation<ImportedSpeaker>,
) -> TranscriptEntryKind {
    match speaker {
        ImportedSourceAttestation::Attested(ImportedSpeaker::User) => TranscriptEntryKind::User,
        ImportedSourceAttestation::Attested(ImportedSpeaker::Assistant) => {
            TranscriptEntryKind::Assistant
        }
        ImportedSourceAttestation::AttestedAbsent | ImportedSourceAttestation::NotAttested => {
            TranscriptEntryKind::System
        }
    }
}

struct TranscriptPageBuilder {
    entries: Vec<TranscriptEntry>,
    max_entries: usize,
    remaining_bytes: usize,
    content_truncated: bool,
}

impl TranscriptPageBuilder {
    fn new(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            entries: Vec::with_capacity(max_entries),
            max_entries,
            remaining_bytes: max_bytes,
            content_truncated: false,
        }
    }

    fn push(&mut self, visible: VisibleEntry) -> bool {
        if self.entries.len() == self.max_entries
            || self.remaining_bytes == 0
            || self.content_truncated
        {
            return false;
        }
        let mut content = visible.content;
        if content.len() > self.remaining_bytes {
            let boundary = content
                .char_indices()
                .map(|(index, _)| index)
                .take_while(|index| *index <= self.remaining_bytes)
                .last()
                .unwrap_or(0);
            content.truncate(boundary);
            self.content_truncated = true;
        }
        self.remaining_bytes = self.remaining_bytes.saturating_sub(content.len());
        self.entries.push(TranscriptEntry::new(
            visible.position,
            visible.kind,
            content,
            self.content_truncated,
        ));
        true
    }

    fn finish(self, has_more: bool) -> TranscriptPage {
        TranscriptPage::new(self.entries, has_more)
    }
}

#[cfg(test)]
mod tests {
    use signalbox_domain::ImportedMessageContentAbsence;

    use super::*;

    #[test]
    fn non_text_import_with_attested_user_speaker_is_a_system_marker() {
        let speaker = ImportedSourceAttestation::Attested(ImportedSpeaker::User);
        let content = ImportedTranscriptContent::MessageContentAbsent(
            ImportedMessageContentAbsence::EmptyBlockArray,
        );

        assert_eq!(
            imported_content_kind(&speaker, &content),
            TranscriptEntryKind::System
        );
    }
}

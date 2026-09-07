use super::*;

/// One complete bounded `list_session_metadata` request.

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SessionMetadataPageRequest {
    pub(crate) required_tags: Vec<String>,
    pub(crate) title_contains: Option<String>,
    pub(crate) include_archived: bool,
    pub(crate) page_size: CanonicalU64,
    pub(crate) after_session_id: Option<CanonicalUuid>,
}

impl SessionMetadataPageRequest {
    pub(crate) fn request(&self) -> ClientRequest {
        ClientRequest::ListSessionMetadata {
            required_tags: self.required_tags.clone(),
            title_contains: self.title_contains.clone(),
            include_archived: self.include_archived,
            page_size: self.page_size,
            after_session_id: self.after_session_id,
        }
    }
}

/// One complete bounded `list_conversations` request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ConversationsPageRequest {
    pub(crate) title_contains: Option<String>,
    pub(crate) origin: ConversationOriginFilter,
    pub(crate) include_archived: bool,
    pub(crate) page_size: CanonicalU64,
    pub(crate) after: Option<ConversationCursor>,
}

impl ConversationsPageRequest {
    pub(crate) fn request(&self) -> ClientRequest {
        ClientRequest::ListConversations {
            title_contains: self.title_contains.clone(),
            origin: self.origin,
            include_archived: self.include_archived,
            page_size: self.page_size,
            after: self.after,
        }
    }
}

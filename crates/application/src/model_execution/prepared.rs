use super::{
    BTreeMap, BTreeSet, BlobDigest, ContextFrontierProjection, MAX_RETAINED_FRONTIER_CONTENT_BYTES,
    ModelCallCredentialReference, ModelConversationMessage, ModelFrontierRenderingError,
    ModelUserContentPart, PreparedModelCallRequest, ProviderReasoningProvenance,
    ResolvedToolConversationEntry, SessionSystemPrompt, ToolDefinition,
    projected_frontier_content_bytes, render_frontier_messages_with_placements,
};

/// A checked prepared call plus its provider-neutral ordered messages.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedModelOperation {
    request: PreparedModelCallRequest,
    credential_reference: ModelCallCredentialReference,
    pub(super) retained_mapped_target: Option<signalbox_domain::ResolvedProviderTarget>,
    system_prompt: Option<SessionSystemPrompt>,
    messages: Box<[ModelConversationMessage]>,
    reasoning_provenance: Box<[ProviderReasoningProvenance]>,
    tools: Box<[ToolDefinition]>,
}

impl PreparedModelOperation {
    /// Renders one checked call request through the canonical frontier projection.
    ///
    /// Retained frontier content is bounded by
    /// `MAX_RETAINED_FRONTIER_CONTENT_BYTES`.
    pub fn render(
        request: PreparedModelCallRequest,
        credential_reference: ModelCallCredentialReference,
        system_prompt: Option<SessionSystemPrompt>,
        tools: Box<[ToolDefinition]>,
        tool_entries: &[ResolvedToolConversationEntry],
        reasoning_provenance: &[ProviderReasoningProvenance],
    ) -> Result<Self, ModelFrontierRenderingError> {
        Self::render_within(
            request,
            credential_reference,
            system_prompt,
            tools,
            tool_entries,
            reasoning_provenance,
            MAX_RETAINED_FRONTIER_CONTENT_BYTES,
        )
    }

    /// Renders under an explicit retained-frontier-content ceiling.
    ///
    /// The ceiling is checked once the projection names its entries and before
    /// any of their content is cloned, so an over-bound frontier is refused
    /// without first materializing the messages that would exhaust memory. The
    /// projection reads the durable frontier by reference for exactly that
    /// reason: naming the entries must not duplicate them.
    /// Taking the ceiling as an argument lets the bound be exercised without
    /// materializing hundreds of megabytes of content.
    pub(super) fn render_within(
        request: PreparedModelCallRequest,
        credential_reference: ModelCallCredentialReference,
        system_prompt: Option<SessionSystemPrompt>,
        tools: Box<[ToolDefinition]>,
        tool_entries: &[ResolvedToolConversationEntry],
        reasoning_provenance: &[ProviderReasoningProvenance],
        retained_frontier_content_limit: usize,
    ) -> Result<Self, ModelFrontierRenderingError> {
        // Borrowed, not copied: an owning collection of the frontier would
        // duplicate every payload's content before the ceiling below could
        // refuse it, which is the allocation the ceiling exists to prevent.
        let complete_entries = request.frontier_entry_slice();
        let projection = ContextFrontierProjection::from_complete_entries(complete_entries)
            .map_err(ModelFrontierRenderingError::InvalidContextProjection)?;
        let entries_by_reference = complete_entries
            .iter()
            .map(|entry| (entry.reference(), entry))
            .collect::<BTreeMap<_, _>>();
        let projected_references = projection.ordered_entries().collect::<BTreeSet<_>>();
        let mut projected_entries = Vec::with_capacity(projected_references.len());
        for reference in projection.ordered_entries() {
            let Some(entry) = entries_by_reference.get(&reference) else {
                return Err(ModelFrontierRenderingError::MissingProjectedEntry {
                    entry: reference,
                });
            };
            projected_entries.push((reference, entry.payload()));
        }
        let projected_tool_entries = tool_entries
            .iter()
            .filter(|entry| projected_references.contains(&entry.source()));
        // Enforced here, between naming the projection and cloning it: the
        // rendered messages are what would exhaust memory, so the refusal has
        // to precede their construction rather than follow it. Everything read
        // to reach this point is a borrow of already-resident durable facts.
        let observed_bytes = projected_frontier_content_bytes(
            projected_entries.iter().copied(),
            |accepted_input| request.origin_content(accepted_input),
            projected_tool_entries.clone(),
        );
        if observed_bytes > retained_frontier_content_limit {
            return Err(
                ModelFrontierRenderingError::RetainedFrontierContentLimitExceeded {
                    observed_bytes,
                    limit_bytes: retained_frontier_content_limit,
                },
            );
        }
        let messages = render_frontier_messages_with_placements(
            projected_entries,
            |accepted_input| request.origin_content(accepted_input).cloned(),
            |digest| request.attachment_byte_length(digest),
            projected_tool_entries,
            |source, revision| request.runner_placement_sandbox(source, revision),
        )?;
        let mut provenance_by_source = BTreeMap::new();
        for provenance in reasoning_provenance {
            if provenance_by_source
                .insert(provenance.source, provenance)
                .is_some()
            {
                return Err(
                    ModelFrontierRenderingError::MissingOrMismatchedReasoningProvenance {
                        entry: provenance.source,
                    },
                );
            }
        }
        let mut retained_provenance = Vec::new();
        for message in &messages {
            if let ModelConversationMessage::ProviderReasoning {
                source,
                producing_call,
                ..
            } = message
            {
                let provenance = provenance_by_source
                    .get(source)
                    .filter(|provenance| provenance.producing_call == *producing_call)
                    .ok_or(
                        ModelFrontierRenderingError::MissingOrMismatchedReasoningProvenance {
                            entry: *source,
                        },
                    )?;
                retained_provenance.push((**provenance).clone());
            }
        }
        Ok(Self {
            retained_mapped_target: None,
            request,
            credential_reference,
            system_prompt,
            messages,
            reasoning_provenance: retained_provenance.into_boxed_slice(),
            tools,
        })
    }

    /// Returns a retained serving target with its fast-mode mapping already applied.
    pub const fn retained_mapped_target(&self) -> Option<signalbox_domain::ResolvedProviderTarget> {
        self.retained_mapped_target
    }

    /// Borrows the checked durable request facts.
    pub const fn request(&self) -> &PreparedModelCallRequest {
        &self.request
    }

    /// Borrows the producing-call facts for the projected reasoning items.
    pub fn reasoning_provenance(&self) -> &[ProviderReasoningProvenance] {
        &self.reasoning_provenance
    }

    /// Borrows the exact durable credential reference pinned with the call.
    pub const fn credential_reference(&self) -> &ModelCallCredentialReference {
        &self.credential_reference
    }

    /// Borrows the exact session system prompt frozen through the turn's
    /// defaults epoch, when that epoch carries one.
    pub fn system_prompt(&self) -> Option<&str> {
        self.system_prompt.as_ref().map(SessionSystemPrompt::as_str)
    }

    /// Borrows the exact messages in frontier order.
    pub fn messages(&self) -> &[ModelConversationMessage] {
        &self.messages
    }

    /// Borrows the exact model-facing catalog snapshot.
    pub fn tools(&self) -> &[ToolDefinition] {
        &self.tools
    }

    /// Iterates over attachment digests represented by the rendered request.
    pub fn attachment_digests(&self) -> impl Iterator<Item = BlobDigest> + '_ {
        self.messages
            .iter()
            .filter_map(|message| match message {
                ModelConversationMessage::User { content, .. } => Some(content.parts()),
                _ => None,
            })
            .flatten()
            .filter_map(|part| match part {
                ModelUserContentPart::AttachmentStub(stub) => Some(stub.digest),
                ModelUserContentPart::Text(_) => None,
            })
    }
}

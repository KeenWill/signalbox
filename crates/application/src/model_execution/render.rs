use super::{
    AcceptedInputId, AttachmentKind, BTreeMap, BlobDigest, ClassifyOperatorFailure,
    ContextFrontierProjectionFailure, DelegationWaitMode, ImportedSourceAttestation,
    ImportedSpeaker, ImportedTranscriptContent, MAX_RENDERED_ATTACHMENT_STUB_BYTES,
    ModelAttachmentStub, ModelConversationMessage, ModelToolResultContent, ModelUserContent,
    ModelUserContentPart, NonZeroU64, OperatorFailureClass, ResolvedToolConversationEntry,
    SemanticTranscriptEntryPayload, SemanticTranscriptEntryRef, SerializedAttachmentEnvelope,
    SerializedAttachmentStub, ToolApprovalDecision, ToolAttemptEnd, ToolResultContent, UserContent,
    UserContentPart,
};

/// Renders ordered user content into canonical provider-visible text and
/// bounded attachment stubs.
pub fn render_model_user_content(
    content: UserContent,
    mut attachment_byte_length: impl FnMut(BlobDigest) -> Option<NonZeroU64>,
) -> Result<ModelUserContent, ModelFrontierRenderingError> {
    let parts = content
        .into_parts()
        .into_iter()
        .map(|part| match part {
            UserContentPart::Text { value } => Ok(ModelUserContentPart::Text(value)),
            UserContentPart::Attachment {
                digest,
                kind,
                media_type,
                display_filename,
            } => {
                let byte_length = attachment_byte_length(digest)
                    .ok_or(ModelFrontierRenderingError::MissingAttachmentBlobFact { digest })?;
                let kind_name = match kind {
                    AttachmentKind::Image => "image",
                    AttachmentKind::Document => "document",
                    AttachmentKind::File => "file",
                };
                let serialized = serde_json::to_string(&SerializedAttachmentEnvelope {
                    signalbox_attachment: SerializedAttachmentStub {
                        kind: kind_name,
                        media_type: media_type.as_str(),
                        display_filename: display_filename
                            .as_ref()
                            .map(signalbox_domain::AttachmentDisplayFilename::as_str),
                        byte_length: byte_length.get().to_string(),
                        digest: digest.to_string(),
                    },
                })
                .map_err(|_| ModelFrontierRenderingError::AttachmentStubSerialization)?;
                if serialized.len() > MAX_RENDERED_ATTACHMENT_STUB_BYTES {
                    return Err(ModelFrontierRenderingError::AttachmentStubBoundExceeded);
                }
                Ok(ModelUserContentPart::AttachmentStub(ModelAttachmentStub {
                    rendered: serialized,
                    digest,
                    kind,
                    media_type,
                    display_filename,
                }))
            }
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Vec::into_boxed_slice)?;
    Ok(ModelUserContent { parts })
}

#[cfg(test)]
pub(super) fn render_frontier_messages<'a>(
    entries: impl IntoIterator<
        Item = (
            SemanticTranscriptEntryRef,
            &'a SemanticTranscriptEntryPayload,
        ),
    >,
    origin_content: impl FnMut(AcceptedInputId) -> Option<UserContent>,
    attachment_byte_length: impl FnMut(BlobDigest) -> Option<NonZeroU64>,
    tool_entries: impl IntoIterator<Item = &'a ResolvedToolConversationEntry>,
) -> Result<Box<[ModelConversationMessage]>, ModelFrontierRenderingError> {
    render_frontier_messages_with_placements(
        entries,
        origin_content,
        attachment_byte_length,
        tool_entries,
        |_, _| None,
    )
}

pub(super) fn render_frontier_messages_with_placements<'a>(
    entries: impl IntoIterator<
        Item = (
            SemanticTranscriptEntryRef,
            &'a SemanticTranscriptEntryPayload,
        ),
    >,
    mut origin_content: impl FnMut(AcceptedInputId) -> Option<UserContent>,
    mut attachment_byte_length: impl FnMut(BlobDigest) -> Option<NonZeroU64>,
    tool_entries: impl IntoIterator<Item = &'a ResolvedToolConversationEntry>,
    mut runner_placement: impl FnMut(
        SemanticTranscriptEntryRef,
        signalbox_domain::RunnerGeneration,
    ) -> Option<signalbox_domain::RunnerSandboxProfile>,
) -> Result<Box<[ModelConversationMessage]>, ModelFrontierRenderingError> {
    let mut resolved_tools = BTreeMap::new();
    for evidence in tool_entries {
        if resolved_tools.insert(evidence.source(), evidence).is_some() {
            return Err(ModelFrontierRenderingError::DuplicateToolEvidence {
                entry: evidence.source(),
            });
        }
    }
    let mut messages = Vec::new();
    for (source, payload) in entries {
        match payload {
            SemanticTranscriptEntryPayload::RunnerPlacementChanged { placement_revision } => {
                let sandbox = runner_placement(source, *placement_revision).ok_or(
                    ModelFrontierRenderingError::MissingOrMismatchedPlacementEvidence {
                        entry: source,
                    },
                )?;
                messages.push(ModelConversationMessage::RunnerPlacementChanged {
                    source,
                    placement_revision: *placement_revision,
                    sandbox,
                });
            }
            SemanticTranscriptEntryPayload::Imported {
                imported_entry,
                source_speaker: ImportedSourceAttestation::Attested(ImportedSpeaker::User),
                content:
                    ImportedTranscriptContent::Text(ImportedSourceAttestation::Attested(content)),
            } => messages.push(ModelConversationMessage::ImportedUser {
                source,
                imported_entry: *imported_entry,
                content: content.clone(),
            }),
            SemanticTranscriptEntryPayload::Imported {
                imported_entry,
                source_speaker: ImportedSourceAttestation::Attested(ImportedSpeaker::Assistant),
                content:
                    ImportedTranscriptContent::Text(ImportedSourceAttestation::Attested(content)),
            } => messages.push(ModelConversationMessage::ImportedAssistant {
                source,
                imported_entry: *imported_entry,
                content: content.clone(),
            }),
            SemanticTranscriptEntryPayload::Imported { .. } => {}
            SemanticTranscriptEntryPayload::ModelIdentityChanged {
                defaults_version,
                selected,
                ..
            } => messages.push(ModelConversationMessage::ModelIdentityChanged {
                source,
                defaults_version: *defaults_version,
                selected: *selected,
            }),
            SemanticTranscriptEntryPayload::ContextSummary {
                producing_call,
                summarized,
                value,
            } => messages.push(ModelConversationMessage::ContextSummary {
                source,
                producing_call: *producing_call,
                summarized: *summarized,
                content: value.clone(),
            }),
            SemanticTranscriptEntryPayload::OriginAcceptedInput { accepted_input }
            | SemanticTranscriptEntryPayload::SteeringAcceptedInput { accepted_input, .. } => {
                let content = origin_content(*accepted_input).ok_or(
                    ModelFrontierRenderingError::MissingOriginContent {
                        entry: source,
                        accepted_input: *accepted_input,
                    },
                )?;
                let content = render_model_user_content(content, &mut attachment_byte_length)?;
                messages.push(ModelConversationMessage::User {
                    source,
                    accepted_input: *accepted_input,
                    content,
                });
            }
            SemanticTranscriptEntryPayload::DelegatedTask {
                spawning_request,
                parent_session,
                parent_turn,
                content,
            } => messages.push(ModelConversationMessage::DelegatedTask {
                source,
                spawning_request: *spawning_request,
                parent_session: *parent_session,
                parent_turn: *parent_turn,
                content: content.clone(),
            }),
            SemanticTranscriptEntryPayload::DelegationMessage {
                spawning_request,
                message,
                sender,
                recipient,
                delivery_sequence,
                content,
            } => messages.push(ModelConversationMessage::DelegationMessage {
                source,
                spawning_request: *spawning_request,
                message: *message,
                sender: *sender,
                recipient: *recipient,
                delivery_sequence: *delivery_sequence,
                content: content.clone(),
            }),
            SemanticTranscriptEntryPayload::DelegationResult {
                awaiting_request,
                spawning_request,
                child,
                mode,
                delivery_sequence,
                outcome,
            } => match (mode, delivery_sequence) {
                (DelegationWaitMode::Foreground, None) => {
                    messages.push(ModelConversationMessage::ToolResult {
                        source,
                        request: *awaiting_request,
                        content: ModelToolResultContent::Delegation(outcome.as_ref().clone()),
                    });
                }
                (DelegationWaitMode::Background, Some(delivery_sequence)) => {
                    messages.push(ModelConversationMessage::BackgroundDelegationResult {
                        source,
                        awaiting_request: *awaiting_request,
                        spawning_request: *spawning_request,
                        child: *child,
                        delivery_sequence: *delivery_sequence,
                        outcome: outcome.as_ref().clone(),
                    });
                }
                (DelegationWaitMode::Foreground, Some(_))
                | (DelegationWaitMode::Background, None) => {
                    return Err(ModelFrontierRenderingError::InvalidDelegationDelivery {
                        entry: source,
                    });
                }
            },
            SemanticTranscriptEntryPayload::AssistantText {
                producing_call,
                value,
            } => messages.push(ModelConversationMessage::Assistant {
                source,
                producing_call: *producing_call,
                content: value.clone(),
            }),
            SemanticTranscriptEntryPayload::ProviderCompaction {
                producing_call,
                block,
            } => messages.push(ModelConversationMessage::ProviderCompaction {
                source,
                producing_call: *producing_call,
                block: block.clone(),
            }),
            SemanticTranscriptEntryPayload::ProviderReasoning {
                producing_call,
                item,
            } => {
                messages.push(ModelConversationMessage::ProviderReasoning {
                    source,
                    producing_call: *producing_call,
                    item: item.clone(),
                });
            }
            SemanticTranscriptEntryPayload::AssistantToolUse {
                producing_call,
                request,
            } => {
                let Some(ResolvedToolConversationEntry::AssistantToolUse {
                    request: record, ..
                }) = resolved_tools.remove(&source)
                else {
                    return Err(
                        ModelFrontierRenderingError::MissingOrMismatchedToolEvidence {
                            entry: source,
                        },
                    );
                };
                if record.id() != *request
                    || record.producing_call() != *producing_call
                    || record.session() != source.source_session()
                {
                    return Err(
                        ModelFrontierRenderingError::MissingOrMismatchedToolEvidence {
                            entry: source,
                        },
                    );
                }
                messages.push(ModelConversationMessage::AssistantToolUse {
                    source,
                    producing_call: *producing_call,
                    request: record.clone(),
                });
            }
            SemanticTranscriptEntryPayload::ToolExecutionResult { attempt } => {
                let Some(ResolvedToolConversationEntry::ExecutionResult {
                    request,
                    attempt: ended,
                    ..
                }) = resolved_tools.remove(&source)
                else {
                    return Err(
                        ModelFrontierRenderingError::MissingOrMismatchedToolEvidence {
                            entry: source,
                        },
                    );
                };
                if ended.attempt() != *attempt
                    || ended.request() != request.id()
                    || ended.session() != source.source_session()
                    || ended.turn() != request.turn()
                    || request.session() != source.source_session()
                {
                    return Err(
                        ModelFrontierRenderingError::MissingOrMismatchedToolEvidence {
                            entry: source,
                        },
                    );
                }
                let content = match ended.end() {
                    ToolAttemptEnd::Completed { result } => {
                        ModelToolResultContent::Success(result.clone())
                    }
                    ToolAttemptEnd::KnownFailed { error } => {
                        ModelToolResultContent::ExecutionError(error.clone())
                    }
                    ToolAttemptEnd::AwaitingChild { .. } | ToolAttemptEnd::Ambiguous => {
                        return Err(ModelFrontierRenderingError::UnrenderableToolResult {
                            entry: source,
                        });
                    }
                };
                messages.push(ModelConversationMessage::ToolResult {
                    source,
                    request: request.id(),
                    content,
                });
            }
            SemanticTranscriptEntryPayload::ToolDenied { request } => {
                let Some(ResolvedToolConversationEntry::Denied {
                    request: record,
                    approval,
                    ..
                }) = resolved_tools.remove(&source)
                else {
                    return Err(
                        ModelFrontierRenderingError::MissingOrMismatchedToolEvidence {
                            entry: source,
                        },
                    );
                };
                let ToolApprovalDecision::Deny { reason } = approval.decision() else {
                    return Err(
                        ModelFrontierRenderingError::MissingOrMismatchedToolEvidence {
                            entry: source,
                        },
                    );
                };
                if record.id() != *request
                    || approval.request() != *request
                    || record.session() != source.source_session()
                {
                    return Err(
                        ModelFrontierRenderingError::MissingOrMismatchedToolEvidence {
                            entry: source,
                        },
                    );
                }
                messages.push(ModelConversationMessage::ToolResult {
                    source,
                    request: *request,
                    content: ModelToolResultContent::Denied {
                        reason: reason.clone(),
                    },
                });
            }
            SemanticTranscriptEntryPayload::ToolInadmissible { request } => {
                let Some(ResolvedToolConversationEntry::Inadmissible {
                    request: record, ..
                }) = resolved_tools.remove(&source)
                else {
                    return Err(
                        ModelFrontierRenderingError::MissingOrMismatchedToolEvidence {
                            entry: source,
                        },
                    );
                };
                if record.id() != *request || record.session() != source.source_session() {
                    return Err(
                        ModelFrontierRenderingError::MissingOrMismatchedToolEvidence {
                            entry: source,
                        },
                    );
                }
                let Some(reason) = record.inadmissible_reason() else {
                    return Err(
                        ModelFrontierRenderingError::MissingOrMismatchedToolEvidence {
                            entry: source,
                        },
                    );
                };
                messages.push(ModelConversationMessage::ToolResult {
                    source,
                    request: *request,
                    content: ModelToolResultContent::ExecutionError(reason.execution_error()),
                });
            }
            SemanticTranscriptEntryPayload::ToolClosed { request } => {
                let Some(ResolvedToolConversationEntry::Closed {
                    request: record, ..
                }) = resolved_tools.remove(&source)
                else {
                    return Err(
                        ModelFrontierRenderingError::MissingOrMismatchedToolEvidence {
                            entry: source,
                        },
                    );
                };
                if record.id() != *request || record.session() != source.source_session() {
                    return Err(
                        ModelFrontierRenderingError::MissingOrMismatchedToolEvidence {
                            entry: source,
                        },
                    );
                }
                messages.push(ModelConversationMessage::ToolResult {
                    source,
                    request: *request,
                    content: ModelToolResultContent::ClosedByTurnEnd,
                });
            }
            SemanticTranscriptEntryPayload::TurnFailed { .. }
            | SemanticTranscriptEntryPayload::TurnCancelled { .. }
            | SemanticTranscriptEntryPayload::TurnCompleted { .. } => {}
        }
    }
    if let Some(entry) = resolved_tools.into_keys().next() {
        return Err(ModelFrontierRenderingError::UnexpectedToolEvidence { entry });
    }
    Ok(messages.into_boxed_slice())
}

/// Sums the model-visible content one render would clone into messages.
///
/// Every term mirrors exactly what `render_frontier_messages` clones for that
/// entry shape, across both content sources it draws from: the projected
/// payloads themselves and the resolved tool evidence they name. Payloads
/// contribute attested imported text, origin and steering user content,
/// delegated task and peer-message content, delivered delegation-outcome
/// content, context-summary text, and assistant text; evidence contributes a
/// proposal's request arguments, a result's result text or error detail, and a
/// denial's reason. Counting only the tool evidence would leave assistant text
/// — which carries no length bound of its own — outside a ceiling that clones
/// it, so the sum has to span every kind the renderer clones or the bound is
/// not the bound it names.
///
/// A shape the renderer skips or refuses contributes nothing, because it clones
/// nothing: unattested or non-text imported content, a delegation result whose
/// wait mode contradicts its delivery position, and turn markers all render no
/// content. A result entry contributes no arguments because its message carries
/// only the request identity, so a request's arguments are counted once through
/// its proposal. Fixed-width identities and the separately bounded tool name a
/// proposal carries are outside the sum: they do not scale with admitted
/// content, and the ceiling exists to bound what does.
///
/// Reading the lengths of already-resident durable facts allocates nothing,
/// which is what lets the ceiling be enforced before the clone rather than
/// after it.
///
/// Sums the text a user-content part array carries.
///
/// Ordered user content holds text parts and attachment parts. Only the text
/// parts carry bytes that scale with what the renderer clones; an attachment
/// part carries a fixed-width digest, a bounded media-type declaration, and an
/// optional bounded display filename, all of which sit outside this sum for the
/// same reason the fixed-width identities do. Exactly one text part reduces
/// this to the single-text length the ceiling counted before user content grew
/// a part array, so the bound does not move for content that did not change
/// shape.
fn user_content_text_bytes(content: &UserContent) -> usize {
    content
        .parts()
        .iter()
        .fold(0_usize, |total, part| match part {
            UserContentPart::Text { value } => total.saturating_add(value.as_str().len()),
            UserContentPart::Attachment { .. } => total,
        })
}

pub(super) fn projected_frontier_content_bytes<'a>(
    entries: impl IntoIterator<
        Item = (
            SemanticTranscriptEntryRef,
            &'a SemanticTranscriptEntryPayload,
        ),
    >,
    mut origin_content: impl FnMut(AcceptedInputId) -> Option<&'a UserContent>,
    tool_entries: impl IntoIterator<Item = &'a ResolvedToolConversationEntry>,
) -> usize {
    let payload_bytes = entries.into_iter().fold(0_usize, |total, (_, payload)| {
        let bytes = match payload {
            SemanticTranscriptEntryPayload::Imported {
                source_speaker:
                    ImportedSourceAttestation::Attested(
                        ImportedSpeaker::User | ImportedSpeaker::Assistant,
                    ),
                content:
                    ImportedTranscriptContent::Text(ImportedSourceAttestation::Attested(content)),
                ..
            } => content.as_str().len(),
            // Every other imported shape renders no message at all.
            SemanticTranscriptEntryPayload::Imported { .. } => 0,
            SemanticTranscriptEntryPayload::OriginAcceptedInput { accepted_input }
            | SemanticTranscriptEntryPayload::SteeringAcceptedInput { accepted_input, .. } => {
                // Absent origin content refuses the render instead of cloning.
                origin_content(*accepted_input).map_or(0, user_content_text_bytes)
            }
            SemanticTranscriptEntryPayload::DelegatedTask { content, .. }
            | SemanticTranscriptEntryPayload::DelegationMessage { content, .. } => {
                content.as_str().len()
            }
            SemanticTranscriptEntryPayload::DelegationResult {
                mode,
                delivery_sequence,
                outcome,
                ..
            } => match (mode, delivery_sequence) {
                (DelegationWaitMode::Foreground, None)
                | (DelegationWaitMode::Background, Some(_)) => outcome
                    .content()
                    .map_or(0, |content| content.as_str().len()),
                // Contradictory delivery is refused, so nothing is cloned.
                (DelegationWaitMode::Foreground, Some(_))
                | (DelegationWaitMode::Background, None) => 0,
            },
            SemanticTranscriptEntryPayload::ContextSummary { value, .. }
            | SemanticTranscriptEntryPayload::AssistantText { value, .. } => value.as_str().len(),
            SemanticTranscriptEntryPayload::ProviderCompaction { block, .. } => {
                block.as_json().len()
            }
            SemanticTranscriptEntryPayload::ProviderReasoning { item, .. } => item.as_json().len(),
            // Identity-only payloads carry no content of their own. Tool
            // payloads name evidence rather than carrying it, and that
            // evidence is summed below.
            SemanticTranscriptEntryPayload::RunnerPlacementChanged { .. }
            | SemanticTranscriptEntryPayload::ModelIdentityChanged { .. }
            | SemanticTranscriptEntryPayload::AssistantToolUse { .. }
            | SemanticTranscriptEntryPayload::ToolExecutionResult { .. }
            | SemanticTranscriptEntryPayload::ToolDenied { .. }
            | SemanticTranscriptEntryPayload::ToolInadmissible { .. }
            | SemanticTranscriptEntryPayload::ToolClosed { .. }
            | SemanticTranscriptEntryPayload::TurnFailed { .. }
            | SemanticTranscriptEntryPayload::TurnCancelled { .. }
            | SemanticTranscriptEntryPayload::TurnCompleted { .. } => 0,
        };
        total.saturating_add(bytes)
    });
    tool_entries
        .into_iter()
        .fold(payload_bytes, |total, entry| {
            let bytes = match entry {
                ResolvedToolConversationEntry::AssistantToolUse { request, .. } => {
                    request.arguments().as_str().len()
                }
                ResolvedToolConversationEntry::ExecutionResult { attempt, .. } => {
                    match attempt.end() {
                        ToolAttemptEnd::Completed { result } => match result {
                            ToolResultContent::Text(text) => text.as_str().len(),
                        },
                        ToolAttemptEnd::KnownFailed { error } => {
                            error.detail().map_or(0, |detail| detail.as_str().len())
                        }
                        // Neither shape renders, so neither retains content.
                        ToolAttemptEnd::AwaitingChild { .. } | ToolAttemptEnd::Ambiguous => 0,
                    }
                }
                ResolvedToolConversationEntry::Denied { approval, .. } => match approval.decision()
                {
                    ToolApprovalDecision::Deny { reason } => {
                        reason.as_ref().map_or(0, |reason| reason.as_str().len())
                    }
                    ToolApprovalDecision::Approve => 0,
                },
                // A closed request renders a fixed marker carrying no content.
                ResolvedToolConversationEntry::Closed { .. } => 0,
                ResolvedToolConversationEntry::Inadmissible { request, .. } => {
                    request.inadmissible_reason().map_or(0, |reason| {
                        reason
                            .execution_error()
                            .detail()
                            .map_or(0, |detail| detail.as_str().len())
                    })
                }
            };
            total.saturating_add(bytes)
        })
}

#[derive(signalbox_derive::OperatorError)]
/// A checked frontier could not be projected into the current text-only input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelFrontierRenderingError {
    #[error("model frontier placement evidence is missing or mismatched")]
    /// The placement reference lacks its exact checked successor record.
    MissingOrMismatchedPlacementEvidence {
        /// Source-qualified placement entry.
        entry: SemanticTranscriptEntryRef,
    },
    /// A projected reasoning item lacks one exact producing-call fact record.
    #[error("missing or mismatched provider reasoning provenance")]
    MissingOrMismatchedReasoningProvenance {
        /// The source-qualified item whose provenance is inconsistent.
        entry: SemanticTranscriptEntryRef,
    },
    #[error("model frontier origin content is missing")]
    /// A frontier origin was missing its reconstituted accepted-input content.
    MissingOriginContent {
        /// The source-qualified origin entry.
        entry: SemanticTranscriptEntryRef,
        /// The accepted input whose content was absent.
        accepted_input: AcceptedInputId,
    },
    #[error("model frontier attachment catalog fact is missing")]
    /// A referenced attachment lacked its immutable catalog length fact.
    MissingAttachmentBlobFact {
        /// Global blob identity whose catalog projection was absent.
        digest: BlobDigest,
    },
    #[error("model frontier attachment stub could not be serialized")]
    /// Canonical attachment metadata could not be serialized.
    AttachmentStubSerialization,
    #[error("model frontier attachment stub exceeded its byte bound")]
    /// Checked attachment metadata exceeded its derived rendered bound.
    AttachmentStubBoundExceeded,
    #[error("model frontier tool evidence is duplicated")]
    /// Two storage evidence values claimed the same semantic entry.
    DuplicateToolEvidence {
        /// Duplicated source-qualified entry.
        entry: SemanticTranscriptEntryRef,
    },
    #[error("model frontier tool evidence is missing or mismatched")]
    /// Reference-only tool history lacks exact correlated durable authority.
    MissingOrMismatchedToolEvidence {
        /// Source-qualified entry whose evidence is absent or cross-wired.
        entry: SemanticTranscriptEntryRef,
    },
    #[error("model frontier contains an unrenderable tool result")]
    /// Durable ambiguity cannot be projected as an ordinary model-visible result.
    UnrenderableToolResult {
        /// Source-qualified result entry.
        entry: SemanticTranscriptEntryRef,
    },
    #[error("model frontier tool evidence is not referenced")]
    /// Storage supplied evidence not named by the checked frontier.
    UnexpectedToolEvidence {
        /// Extra source-qualified entry.
        entry: SemanticTranscriptEntryRef,
    },
    #[error("context projection entry is missing from its frontier")]
    /// A projection named an entry absent from its complete source frontier.
    MissingProjectedEntry {
        /// The absent source-qualified entry.
        entry: SemanticTranscriptEntryRef,
    },
    #[error("model frontier delegation delivery is inconsistent")]
    /// A stored delegation wait mode contradicted its delivery position.
    InvalidDelegationDelivery {
        /// Source-qualified delegation-result entry.
        entry: SemanticTranscriptEntryRef,
    },
    #[error("model frontier retained content exceeds its ceiling")]
    /// The projected frontier content exceeded its retained-content ceiling.
    ///
    /// Raised before any projected content is cloned, so the refusal bounds the
    /// memory the rendered messages would have held.
    RetainedFrontierContentLimitExceeded {
        /// Cumulative projected content bytes the render would have cloned.
        observed_bytes: usize,
        /// The ceiling in force for this render.
        limit_bytes: usize,
    },
    #[error("invalid context-compaction projection")]
    /// The complete durable frontier carries malformed summary provenance.
    InvalidContextProjection(ContextFrontierProjectionFailure),
}

impl ClassifyOperatorFailure for ModelFrontierRenderingError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        OperatorFailureClass::CallerOrHubBug
    }
}

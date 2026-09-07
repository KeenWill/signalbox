//! First text-only model-call execution orchestration.
//!
//! docs/spec/model-call-execution.md owns the staged transaction and
//! provider-effect order. The application keeps persistence, provider
//! capability preparation, send authorization, provider interaction, and
//! terminal observation distinct.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fmt,
    future::Future,
    num::NonZeroU64,
    sync::{Arc, Weak},
    time::Duration,
};

// The configured automatic tool-round ceiling alone does not bound memory: it
// multiplies against the 32-request batch bound and the 1 MiB argument and
// result bounds, so a 256-round deployment would admit 16 GiB of retained
// argument and result text where 32 rounds admitted 2 GiB. Retained content is
// therefore bounded on its own terms, independently of the round ceiling — and
// of whether a deployment configured one at all. One maximal round retains 32
// requests times 1 MiB of arguments plus 1 MiB of results, so this admits four
// maximal rounds while leaving the round ceiling operative for the
// kilobyte-scale results real executors return. It bounds every kind of content
// a render clones, not tool evidence alone: assistant text carries no length
// bound of its own beyond the transport cap on a single response, so a ceiling
// blind to it would be multiplied by the same round count it is meant to
// contain. It also sits far above any provider context window, so it cannot
// refuse a turn a provider would accept.
const MAX_RETAINED_FRONTIER_CONTENT_BYTES: usize = 256 * 1024 * 1024;

// Worst-case compact JSON for maximum checked metadata, u64 length, and digest.
const MAX_RENDERED_ATTACHMENT_STUB_BYTES: usize = 2_304;

use signalbox_domain::{
    AcceptedInputId, AmbiguousModelCallTurnIdentities, AssistantResponsePart, AssistantText,
    AttachmentKind, AuthorizedModelCall, AvailabilitySuccessorModelCallTurn, BlobDigest,
    CompletedModelCallIdentities, ContextCompactionRange, ContextFrontierId,
    ContextFrontierProjection, ContextFrontierProjectionFailure,
    CorrelatedModelCallTerminalObservation, CredentialPoolExhaustedModelCallTurn,
    DangerousToolAutoApproval, DelegationContent, DelegationMessageId, DelegationOutcome,
    DelegationWaitMode, DirectModelSelection, FailedModelCallTurn, FailedModelCallTurnIdentities,
    ImportedSourceAttestation, ImportedSpeaker, ImportedText, ImportedTranscriptContent,
    ImportedTranscriptEntryId, InitialToolApproval, ModelCallId, ModelCallTerminalIdentities,
    ModelCallTerminalObservation, ModelCallTerminalOutcome,
    PhysicalCancellationModelCallTurnIdentities, PreparedModelCallRequest, ProviderCompactionBlock,
    RecordedUserOverride, RefusedModelCallTurnIdentities, SemanticTranscriptEntryId,
    SemanticTranscriptEntryPayload, SemanticTranscriptEntryRef,
    SessionConfigurationDefaultsVersion, SessionId, SessionSystemPrompt,
    StopRequestedModelCallTurn, StoppedToolResponsePartIdentity,
    StoppedToolRoundModelCallIdentities, ToolApprovalDecision, ToolAttemptEnd, ToolDenialReason,
    ToolExecutionError, ToolRequest, ToolRequestId, ToolResponsePartIdentity, ToolResultContent,
    ToolRoundModelCallIdentities, TurnAttemptId, TurnId, UserContent, UserContentPart,
};
use tokio::sync::{Mutex, OwnedMutexGuard};

use crate::{
    ClassifyOperatorFailure, NoToolCatalog, OperatorFailureClass, ResolvedToolConversationEntry,
    ToolCatalog, ToolDefinition, tool_loop::initial_tool_approval,
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
    parts: Box<[ModelUserContentPart]>,
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
    rendered: String,
    digest: BlobDigest,
    kind: AttachmentKind,
    media_type: signalbox_domain::DeclaredMediaType,
    display_filename: Option<signalbox_domain::AttachmentDisplayFilename>,
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
struct SerializedAttachmentEnvelope<'a> {
    signalbox_attachment: SerializedAttachmentStub<'a>,
}

#[derive(serde::Serialize)]
struct SerializedAttachmentStub<'a> {
    kind: &'static str,
    media_type: &'a str,
    display_filename: Option<&'a str>,
    byte_length: String,
    digest: String,
}

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
fn render_frontier_messages<'a>(
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

fn render_frontier_messages_with_placements<'a>(
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

fn projected_frontier_content_bytes<'a>(
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
            };
            total.saturating_add(bytes)
        })
}

/// A checked prepared call plus its provider-neutral ordered messages.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedModelOperation {
    request: PreparedModelCallRequest,
    credential_reference: ModelCallCredentialReference,
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
    fn render_within(
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
            request,
            credential_reference,
            system_prompt,
            messages,
            reasoning_provenance: retained_provenance.into_boxed_slice(),
            tools,
        })
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

/// Result of the authoritative prepare-call transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PrepareModelCallOutcome {
    /// The scheduling hint no longer identifies runnable work.
    NoWork,
    /// A durable availability-successor deadline has not elapsed.
    RetryBackoff(Duration),
    /// No credential-pool member was available for this call-free attempt.
    PoolExhausted(Box<CredentialPoolExhaustedModelCallTurn>),
    /// A new exact `Prepared` call committed; this invocation stops here.
    Checkpointed(ModelCallId),
    /// A previously committed `Prepared` request may prepare its capability.
    Ready {
        /// Checked durable request facts.
        request: Box<PreparedModelCallRequest>,
        /// Non-secret credential reference captured with the call.
        credential_reference: ModelCallCredentialReference,
        /// Frozen dangerous blanket posture for initial request decisions.
        dangerous_tool_auto_approval: DangerousToolAutoApproval,
        /// Recorded, not-yet-consumed user overrides of delegate denials, frozen
        /// for this call in the same transaction as the blanket posture.
        recorded_user_overrides: Box<[RecordedUserOverride]>,
        /// Exact optional session system prompt on the turn's frozen epoch.
        system_prompt: Option<SessionSystemPrompt>,
        /// Exact durable authority for every tool-related frontier entry.
        tool_entries: Box<[ResolvedToolConversationEntry]>,
        /// Durable producing-target and credential facts for retained reasoning.
        reasoning_provenance: Box<[ProviderReasoningProvenance]>,
    },
    /// Immutable target resolution failed and the turn closed atomically.
    TargetUnavailable(Box<FailedModelCallTurn>),
}

/// Authoritative transaction that prepares or reloads one initial model call.
pub trait PrepareModelCallTransaction {
    /// Adapter-specific classified failure.
    type Error: ClassifyOperatorFailure;

    /// Runs the serialized prepare role with fresh application candidates.
    fn prepare<NextSteeringIdentities>(
        &mut self,
        session: SessionId,
        call: ModelCallId,
        failure_identities: FailedModelCallTurnIdentities,
        steering_frontier: ContextFrontierId,
        next_steering_identities: NextSteeringIdentities,
    ) -> impl Future<Output = Result<PrepareModelCallOutcome, Self::Error>> + Send
    where
        NextSteeringIdentities:
            FnMut(AcceptedInputId) -> (SemanticTranscriptEntryId, TurnId) + Send;
}

/// Guarded transaction closing a trustworthy local pre-send failure.
pub trait FailPreparedModelCallTransaction {
    /// Adapter-specific classified failure.
    type Error: ClassifyOperatorFailure;

    /// Closes the exact prepared call without authorizing provider work.
    ///
    /// `next_reclassified_turn` is an application-owned fresh-candidate
    /// supplier. The adapter may call it once for each pending steering input
    /// discovered under its authoritative lock; it must not mint identities.
    fn fail_prepared<NextTurn>(
        &mut self,
        session: SessionId,
        call: ModelCallId,
        cause: PreparedModelCallFailureCause,
        attachment_failure: Option<AttachmentPreparationFailure>,
        identities: FailedModelCallTurnIdentities,
        next_reclassified_turn: NextTurn,
    ) -> impl Future<Output = Result<FailedModelCallTurn, Self::Error>> + Send
    where
        NextTurn: FnMut(AcceptedInputId) -> TurnId + Send;

    /// Rereads whether a retained prepared-call failure closure committed.
    fn reread_failure(
        &mut self,
        session: SessionId,
        call: ModelCallId,
        attachment_failure: Option<AttachmentPreparationFailure>,
    ) -> impl Future<Output = Result<RetainedPreparedFailureStatus, Self::Error>> + Send;
}

/// Application-owned reason for closing a prepared call before provider entry.
///
/// This vocabulary stays separate from provider-runtime cause codes because no
/// physical model call has been dispatched when either variant applies.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreparedModelCallFailureCause {
    /// Provider capability preparation reported a trustworthy local failure.
    CapabilityKnownFailure,
    /// The current turn already contains the maximum automatic tool rounds.
    ToolRoundLimitReached,
}

/// Authoritative status of one retained pre-send prepared-call failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetainedPreparedFailureStatus {
    /// The exact call remains `Prepared`; the closure may be resubmitted.
    Pending,
    /// The exact known-failure closure is already represented durably.
    AlreadyCommitted,
    /// A racing interrupt authoritatively cancelled the prepared call.
    Cancelled,
}

/// Distinct transaction that durably authorizes one physical send.
pub trait AuthorizeModelCallTransaction {
    /// Adapter-specific classified failure.
    type Error: ClassifyOperatorFailure;

    /// Reloads exact authority and commits `Prepared -> InFlight`.
    fn authorize(
        &mut self,
        session: SessionId,
        call: ModelCallId,
    ) -> impl Future<Output = Result<AuthorizeModelCallOutcome, Self::Error>> + Send;

    /// Rereads an authorization whose commit acknowledgement was lost.
    fn reread_after_ambiguous_commit(
        &mut self,
        session: SessionId,
        prepared: &PreparedModelCallRequest,
    ) -> impl Future<Output = Result<ModelCallAuthorizationReread, Self::Error>> + Send;

    /// Returns a same-call signal that resolves when durable state forbids
    /// continuing provider work.
    ///
    /// The returned future owns its adapter state so it can outlive this
    /// borrow and race capability preparation or physical invocation.
    fn cancellation_signal(
        &self,
        session: SessionId,
        call: ModelCallId,
    ) -> impl Future<Output = ()> + Send + 'static;
}

/// Result of freshly rechecking one send-authorization hint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuthorizeModelCallOutcome {
    /// The exact prepared authority is stale or has stopped; no send may begin.
    NoSend,
    /// The exact prepared call committed `InFlight` and may enter its provider.
    Authorized(Box<AuthorizedModelCall>),
}

/// Authoritative state after an ambiguous send-authorization commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelCallAuthorizationReread {
    /// The authorization rolled back and the exact call remains Prepared.
    Prepared,
    /// The authorization committed; this exact issued call was not consumed.
    InFlight(Box<AuthorizedModelCall>),
    /// The authorization committed, but an interrupt stopped it before this
    /// process entered the provider.
    CancellationRequested(Box<StopRequestedModelCallTurn>),
    /// An interrupt already terminalized this exact unsent call as Cancelled.
    Cancelled,
}

/// Fresh identity candidates for a terminal observation.
///
/// A tool-using response carries both legal closures because an interrupt can
/// race after provider acceptance. The authoritative transaction selects the
/// continuing or stopped shape only after locking fresh lifecycle state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelCallTerminalIdentityCandidates {
    /// One lifecycle-independent terminal identity shape.
    Exact(ModelCallTerminalIdentities),
    /// Both legal closures for one tool-using response.
    ToolRound {
        /// Nonterminal same-turn continuation identities.
        continuing: ToolRoundModelCallIdentities,
        /// Applied-interrupt terminal closure identities.
        stopped: StoppedToolRoundModelCallIdentities,
    },
    /// Both legal closures for one classified availability failure.
    ///
    /// Persistence validates the call-pinned pool policy and retry bound under
    /// its lock, then consumes the identities for the authorized ending.
    Availability {
        /// Ordinary terminal failure when policy does not authorize a successor.
        failed: FailedModelCallTurnIdentities,
        /// Fresh physical attempt for an authorized availability successor.
        successor_attempt: TurnAttemptId,
    },
}

/// Fresh transaction committing a provider-neutral terminal observation.
pub trait CommitModelCallObservationTransaction {
    /// Adapter-specific classified failure.
    type Error: ClassifyOperatorFailure;

    /// Reloads issued authority and atomically applies one observation.
    ///
    /// The successor supplier has the same application-owned, adapter-consumed
    /// contract as [`FailPreparedModelCallTransaction::fail_prepared`].
    fn commit_observation<NextTurn>(
        &mut self,
        session: SessionId,
        observation: CorrelatedModelCallTerminalObservation,
        identities: ModelCallTerminalIdentityCandidates,
        next_reclassified_turn: NextTurn,
    ) -> impl Future<Output = Result<Option<ModelCallObservationCommitOutcome>, Self::Error>> + Send
    where
        NextTurn: FnMut(AcceptedInputId) -> TurnId + Send;

    /// Rereads whether one retained terminal observation was committed.
    fn reread_observation(
        &mut self,
        session: SessionId,
        observation: &CorrelatedModelCallTerminalObservation,
    ) -> impl Future<Output = Result<RetainedModelCallObservationStatus, Self::Error>> + Send;
}

/// Authoritative status of one unchanged in-memory terminal observation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetainedModelCallObservationStatus {
    /// The exact issued call still awaits this observation.
    Pending,
    /// The exact observation is already represented durably.
    AlreadyCommitted,
    /// The observation committed and its availability successor is durable.
    ///
    /// Distinct from `AlreadyCommitted` because the turn is still active on
    /// the successor attempt: the caller must keep driving it after the
    /// enclosed remaining delay rather than treating the turn as finished.
    AvailabilitySuccessorCommitted {
        /// Remaining wait before the successor attempt may prepare.
        retry_backoff: Duration,
    },
    /// A newer logical terminal proof made the retained provider result inert.
    DiscardedByLogicalTerminal,
}

/// Opaque same-incarnation evidence retained across a failed orchestration stage.
///
/// This state prevents a later service invocation or explicit composition
/// handoff from repeating credential work, losing proof that provider entry
/// never occurred, or dropping an unchanged terminal observation.
/// docs/spec/model-call-execution.md requires a linear handoff token: callers
/// may move it between service `into_parts` and `from_parts` handoffs, but
/// cannot construct or clone evidence.
///
/// ```compile_fail
/// use signalbox_application::RetainedModelCallExecutionState;
///
/// let _forged = RetainedModelCallExecutionState {};
/// ```
///
/// ```compile_fail
/// use signalbox_application::RetainedModelCallExecutionState;
///
/// fn duplicate(state: RetainedModelCallExecutionState) {
///     let _replayed: RetainedModelCallExecutionState = state.clone();
/// }
/// ```
#[derive(Debug, Eq, PartialEq)]
pub struct RetainedModelCallExecutionState {
    state: RetainedModelCallExecutionStateKind,
}

#[derive(Debug, Eq, PartialEq)]
enum RetainedModelCallExecutionStateKind {
    /// A provider-neutral prepared-call failure remains to be reconciled.
    PreparedFailure {
        /// Session owning the exact prepared call.
        session: SessionId,
        /// Turn closed by the exact prepared call.
        turn: TurnId,
        /// Prepared call whose guarded failure closure remains pending.
        call: ModelCallId,
        /// Exact application reason that must survive the retained retry.
        cause: PreparedModelCallFailureCause,
        /// Distinct attachment-preparation cause, absent for ordinary capability failure.
        attachment_failure: Option<AttachmentPreparationFailure>,
    },
    /// Ambiguous authorization still has same-incarnation proof of no send.
    AuthorizationNonConsumption {
        /// Session owning the exact prepared request.
        session: SessionId,
        /// Unchanged request used to reread whether authorization committed.
        prepared: Box<PreparedModelCallRequest>,
    },
    /// One unchanged provider observation awaits authoritative reconciliation.
    TerminalObservation {
        /// Session owning the exact issued call.
        session: SessionId,
        /// Unchanged correlated observation returned by provider work.
        observation: Box<CorrelatedModelCallTerminalObservation>,
        /// Frozen policy outcomes for each tool proposal, in proposal order.
        tool_approvals: Box<[InitialToolApproval]>,
    },
}

/// Closed result of preparing rendered attachment authority before provider work.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttachmentPreparationFailure {
    /// Distinct rendered attachments exceed the deployment verification bound.
    TooLarge {
        /// Deployment maximum applied before store I/O.
        maximum_bytes: u64,
    },
    /// No recorded replica contains the required attachment.
    Missing,
    /// Recorded replicas were readable but failed identity verification.
    Corrupt,
    /// No replica verified and at least one candidate was temporarily unavailable.
    Unavailable,
}

/// Adapter-local result of credential lookup and capability preparation.
pub enum ModelCallCapabilityPreparation<Capability> {
    /// A call-bound one-shot capability is ready to move into provider work.
    Ready(Capability),
    /// Durable authority changed while the capability was being prepared.
    Cancelled,
    /// A trustworthy ordinary local failure occurred before send authorization.
    KnownFailure,
    /// Attachment preparation could not establish authority for the request.
    AttachmentFailure(AttachmentPreparationFailure),
}

/// Outcome of one provider-native prospective input-token estimate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelCallInputTokenCount {
    /// Provider-reported estimate for the rendered operation.
    Counted(u64),
    /// Authority or caller cancellation won before a count completed.
    Cancelled,
    /// Attachment authority is temporarily unavailable, so the queued turn
    /// must be retried before activation rather than sent without a count.
    AttachmentUnavailable,
    /// Attachment preparation found a definitive request-local failure.
    AttachmentFailure(AttachmentPreparationFailure),
    /// No trustworthy provider-native estimate is available.
    Unavailable,
}

/// Provider adapter boundary for prospective input-token estimation.
pub trait ModelCallInputTokenCounter {
    /// Sanitized adapter-specific classified failure.
    type Error: ClassifyOperatorFailure;

    /// Estimates the same provider-native operation shape later prepared for send.
    fn count_input_tokens<Cancellation>(
        &self,
        operation: PreparedModelOperation,
        cancellation: Cancellation,
    ) -> impl Future<Output = Result<ModelCallInputTokenCount, Self::Error>> + Send
    where
        Cancellation: Future<Output = ()> + Send + 'static;
}

/// Provider adapter boundary surrounding an opaque, one-shot send capability.
pub trait ModelCallProvider {
    /// Adapter-owned capability; application code only moves this value.
    type Capability;
    /// Sanitized adapter-specific classified failure.
    type Error: ClassifyOperatorFailure;

    /// Resolves credentials internally and prepares an exact call capability.
    fn prepare_capability<Cancellation>(
        &mut self,
        operation: PreparedModelOperation,
        cancellation: Cancellation,
    ) -> impl Future<Output = Result<ModelCallCapabilityPreparation<Self::Capability>, Self::Error>> + Send
    where
        Cancellation: Future<Output = ()> + Send + 'static;

    /// Consumes one capability after durable send authorization.
    fn invoke<AcceptancePossible, Cancellation>(
        &mut self,
        authorized: AuthorizedModelCall,
        capability: Self::Capability,
        acceptance_possible: AcceptancePossible,
        cancellation: Cancellation,
    ) -> impl Future<Output = Result<CorrelatedModelCallTerminalObservation, Self::Error>> + Send
    where
        AcceptancePossible: FnOnce() + Send,
        Cancellation: Future<Output = ()> + Send + 'static;
}

/// Supplies all hub-minted execution candidates.
pub trait ModelCallExecutionIdGenerator {
    /// Generates a distinct model-call candidate.
    fn next_model_call_id(&mut self) -> ModelCallId;
    /// Generates a distinct semantic-entry candidate.
    fn next_semantic_entry_id(&mut self) -> SemanticTranscriptEntryId;
    /// Generates a distinct context-frontier candidate.
    fn next_context_frontier_id(&mut self) -> ContextFrontierId;
    /// Generates a distinct logical tool-request candidate.
    fn next_tool_request_id(&mut self) -> ToolRequestId;
    /// Generates a distinct same-turn continuation-attempt candidate.
    fn next_turn_attempt_id(&mut self) -> TurnAttemptId;
    /// Generates a distinct reclassified successor-turn candidate.
    fn next_turn_id(&mut self) -> TurnId;
}

/// Production UUIDv7 generator for model-call execution candidates.
#[derive(Clone, Copy, Debug, Default)]
pub struct UuidV7ModelCallExecutionIdGenerator;

impl ModelCallExecutionIdGenerator for UuidV7ModelCallExecutionIdGenerator {
    fn next_model_call_id(&mut self) -> ModelCallId {
        ModelCallId::from_uuid(uuid::Uuid::now_v7())
    }

    fn next_semantic_entry_id(&mut self) -> SemanticTranscriptEntryId {
        SemanticTranscriptEntryId::from_uuid(uuid::Uuid::now_v7())
    }

    fn next_context_frontier_id(&mut self) -> ContextFrontierId {
        ContextFrontierId::from_uuid(uuid::Uuid::now_v7())
    }

    fn next_tool_request_id(&mut self) -> ToolRequestId {
        ToolRequestId::from_uuid(uuid::Uuid::now_v7())
    }

    fn next_turn_attempt_id(&mut self) -> TurnAttemptId {
        TurnAttemptId::from_uuid(uuid::Uuid::now_v7())
    }

    fn next_turn_id(&mut self) -> TurnId {
        TurnId::from_uuid(uuid::Uuid::now_v7())
    }
}

/// Process-shared ordering gate between dispatch and attempt-stop transitions.
pub trait AttemptDispatchGate {
    /// Opaque permit retained across the provider acceptance-crossing window.
    type Permit: Send;

    /// Acquires exclusive ordering for one physical attempt.
    fn acquire(&self, attempt: TurnAttemptId) -> impl Future<Output = Self::Permit> + Send;
}

/// Cloneable attempt-keyed in-process dispatch gate.
#[derive(Clone, Debug, Default)]
pub struct InProcessAttemptDispatchGate {
    attempts: Arc<Mutex<HashMap<TurnAttemptId, Weak<Mutex<()>>>>>,
}

/// Opaque permit from [`InProcessAttemptDispatchGate`].
pub struct InProcessAttemptDispatchPermit {
    _guard: OwnedMutexGuard<()>,
}

impl AttemptDispatchGate for InProcessAttemptDispatchGate {
    type Permit = InProcessAttemptDispatchPermit;

    fn acquire(&self, attempt: TurnAttemptId) -> impl Future<Output = Self::Permit> + Send {
        let attempts = Arc::clone(&self.attempts);
        async move {
            let attempt_gate = {
                let mut known = attempts.lock().await;
                known.retain(|_, gate| gate.strong_count() > 0);
                known
                    .get(&attempt)
                    .and_then(Weak::upgrade)
                    .unwrap_or_else(|| {
                        let gate = Arc::new(Mutex::new(()));
                        known.insert(attempt, Arc::downgrade(&gate));
                        gate
                    })
            };
            InProcessAttemptDispatchPermit {
                _guard: attempt_gate.lock_owned().await,
            }
        }
    }
}

/// Completed stage of one service invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelCallExecutionOutcome {
    /// The scheduling hint no longer identifies runnable work.
    NoWork,
    /// Durable retry backoff remains before the successor may be prepared.
    RetryBackoff(Duration),
    /// The pool admitted no member; this is not a member provider failure.
    PoolExhausted(Box<CredentialPoolExhaustedOutcome>),
    /// A new prepared checkpoint committed and requires a later invocation.
    Checkpointed(ModelCallId),
    /// Target resolution failed before call creation.
    TargetUnavailable(Box<FailedModelCallTurn>),
    /// A trustworthy local capability failure closed the prepared call.
    CapabilityKnownFailure(Box<FailedModelCallTurn>),
    /// Attachment verification was unavailable; the call remains `Prepared`.
    AttachmentUnavailable,
    /// A retained prepared failure's earlier commit was proven to have landed.
    CapabilityFailureAlreadyCommitted(ModelCallId),
    /// The automatic tool-round limit closed the prepared call and turn.
    ToolRoundLimitReached(Box<FailedModelCallTurn>),
    /// A retained tool-round-limit closure was proven to have landed.
    ToolRoundLimitAlreadyCommitted(ModelCallId),
    /// The provider observation committed its authoritative result.
    ObservationCommitted(Box<ModelCallTerminalOutcome>),
    /// An availability failure committed and left the turn on a fresh attempt.
    AvailabilitySuccessor(Box<AvailabilitySuccessorOutcome>),
    /// A retained observation's earlier commit was proven to have landed.
    ObservationAlreadyCommitted(ModelCallId),
}

#[derive(signalbox_derive::OperatorError)]
/// Failure annotated with the exact orchestration stage that failed.
#[derive(Debug)]
pub enum ModelCallExecutionError<
    PrepareError,
    FailureError,
    AuthorizationError,
    ProviderError,
    ObservationError,
> {
    #[error("model-call prepare stage failed: {field_0}")]
    #[operator(delegate = 0, code = "model_call_prepare")]
    /// The prepare-call transaction failed.
    Prepare(PrepareError),
    #[error("model-call render stage failed: {field_0}")]
    #[operator(delegate = 0, code = "model_call_render")]
    /// Provider-neutral request rendering failed closed.
    Render(ModelFrontierRenderingError),
    #[error("model-call capability stage failed: {field_0}")]
    #[operator(delegate = 0, code = "model_call_capability_preparation")]
    /// Credential lookup or capability preparation failed as an operator error.
    CapabilityPreparation(ProviderError),
    #[error("model-call prepared-failure commit failed: {field_0}")]
    #[operator(delegate = 0, code = "model_call_prepared_failure_commit")]
    /// The guarded prepared-call failure transaction failed.
    PreparedFailureCommit(FailureError),
    #[error("model-call prepared-failure reread failed: {field_0}")]
    #[operator(delegate = 0, code = "model_call_prepared_failure_reread")]
    /// Authoritative reread of a retained prepared-call failure failed.
    PreparedFailureReread(FailureError),
    #[error("model-call authorization stage failed: {field_0}")]
    #[operator(delegate = 0, code = "model_call_authorization")]
    /// Durable send authorization failed.
    Authorization(AuthorizationError),
    #[error("model-call authorization reread failed: {reread_error}")]
    #[operator(delegate = reread_error, code = "model_call_authorization_reread")]
    /// Authoritative reread after an ambiguous authorization also failed.
    AuthorizationReread {
        /// The original commit-ambiguous authorization failure.
        authorization_error: AuthorizationError,
        /// The failure to establish whether authorization committed.
        reread_error: AuthorizationError,
    },
    #[error("model-call authorization reconciliation failed: {field_0}")]
    #[operator(delegate = 0, code = "model_call_authorization_reconciliation")]
    /// A later pass still could not reconcile retained non-consumption proof.
    AuthorizationReconciliation(AuthorizationError),
    #[error("model-call provider stage failed: {field_0}")]
    #[operator(delegate = 0, code = "model_call_provider")]
    /// Provider work produced no trustworthy observation.
    Provider(ProviderError),
    #[error("model-call observation commit failed: {error}")]
    #[operator(delegate = error, code = "model_call_observation_commit")]
    /// The terminal-observation transaction failed.
    ObservationCommit {
        /// The failed observation transaction or authoritative reread.
        error: ObservationError,
        /// The unchanged provider observation retained for a later pass.
        retained_observation: CorrelatedModelCallTerminalObservation,
    },
}

/// Coordinates one staged model-call execution invocation.
pub struct ModelCallExecutionService<
    Ids,
    Prepare,
    Failure,
    Authorization,
    Observation,
    Provider,
    Gate,
> {
    ids: Ids,
    prepare: Prepare,
    failure: Failure,
    authorization: Authorization,
    observation: Observation,
    provider: Provider,
    gate: Gate,
    catalog: Arc<dyn ToolCatalog>,
    retained_state: Option<RetainedModelCallExecutionState>,
    max_automatic_tool_rounds_per_turn: Option<usize>,
    retained_frontier_content_limit: usize,
}

impl<Ids, Prepare, Failure, Authorization, Observation, Provider, Gate>
    ModelCallExecutionService<Ids, Prepare, Failure, Authorization, Observation, Provider, Gate>
{
    /// Composes every purpose-specific effect role.
    #[allow(
        clippy::too_many_arguments,
        reason = "the service keeps each effect role and the required deployment policy explicit"
    )]
    pub fn new(
        ids: Ids,
        prepare: Prepare,
        failure: Failure,
        authorization: Authorization,
        observation: Observation,
        provider: Provider,
        gate: Gate,
        max_automatic_tool_rounds_per_turn: Option<usize>,
    ) -> Self {
        Self {
            ids,
            prepare,
            failure,
            authorization,
            observation,
            provider,
            gate,
            catalog: Arc::new(NoToolCatalog),
            retained_state: None,
            max_automatic_tool_rounds_per_turn,
            retained_frontier_content_limit: MAX_RETAINED_FRONTIER_CONTENT_BYTES,
        }
    }

    /// Replaces the empty compatibility catalog with one tool-capable port.
    pub fn with_tool_catalog(mut self, catalog: impl ToolCatalog + 'static) -> Self {
        self.catalog = Arc::new(catalog);
        self
    }

    /// Narrows the retained-tool-content ceiling for one service.
    ///
    /// Deployments run the module ceiling; this exists so the bound can be
    /// exercised end to end without materializing hundreds of megabytes.
    #[cfg(test)]
    const fn with_retained_frontier_content_limit(mut self, limit: usize) -> Self {
        self.retained_frontier_content_limit = limit;
        self
    }

    /// Reconstitutes an explicitly decomposed service without losing evidence.
    #[allow(clippy::too_many_arguments)]
    pub fn from_parts(
        ids: Ids,
        prepare: Prepare,
        failure: Failure,
        authorization: Authorization,
        observation: Observation,
        provider: Provider,
        gate: Gate,
        catalog: Arc<dyn ToolCatalog>,
        retained_state: Option<RetainedModelCallExecutionState>,
        max_automatic_tool_rounds_per_turn: Option<usize>,
    ) -> Self {
        Self {
            ids,
            prepare,
            failure,
            authorization,
            observation,
            provider,
            gate,
            catalog,
            retained_state,
            max_automatic_tool_rounds_per_turn,
            retained_frontier_content_limit: MAX_RETAINED_FRONTIER_CONTENT_BYTES,
        }
    }

    /// Returns every owned effect role for explicit composition handoff.
    #[allow(
        clippy::type_complexity,
        reason = "the tuple deliberately preserves the service's explicit independently owned composition roles"
    )]
    pub fn into_parts(
        self,
    ) -> (
        Ids,
        Prepare,
        Failure,
        Authorization,
        Observation,
        Provider,
        Gate,
        Arc<dyn ToolCatalog>,
        Option<RetainedModelCallExecutionState>,
        Option<usize>,
    ) {
        (
            self.ids,
            self.prepare,
            self.failure,
            self.authorization,
            self.observation,
            self.provider,
            self.gate,
            self.catalog,
            self.retained_state,
            self.max_automatic_tool_rounds_per_turn,
        )
    }

    /// Borrows same-incarnation evidence awaiting reconciliation.
    pub const fn retained_state(&self) -> Option<&RetainedModelCallExecutionState> {
        self.retained_state.as_ref()
    }

    /// Borrows the exact observation awaiting authoritative reconciliation.
    pub fn retained_observation(&self) -> Option<&CorrelatedModelCallTerminalObservation> {
        match self.retained_state.as_ref().map(|retained| &retained.state) {
            Some(RetainedModelCallExecutionStateKind::TerminalObservation {
                observation, ..
            }) => Some(observation),
            Some(
                RetainedModelCallExecutionStateKind::PreparedFailure { .. }
                | RetainedModelCallExecutionStateKind::AuthorizationNonConsumption { .. },
            )
            | None => None,
        }
    }
}

impl<Ids, Prepare, Failure, Authorization, Observation, Provider, Gate>
    ModelCallExecutionService<Ids, Prepare, Failure, Authorization, Observation, Provider, Gate>
where
    Ids: ModelCallExecutionIdGenerator + Send,
    Prepare: PrepareModelCallTransaction,
    Failure: FailPreparedModelCallTransaction,
    Authorization: AuthorizeModelCallTransaction,
    Observation: CommitModelCallObservationTransaction,
    Provider: ModelCallProvider,
    Gate: AttemptDispatchGate,
{
    /// Runs at most one provider interaction for one authoritative session hint.
    ///
    /// A newly committed `Prepared` checkpoint ends this invocation. A later
    /// invocation reloads it, prepares the opaque capability outside a
    /// transaction, authorizes send while holding the shared attempt gate,
    /// invokes the provider once, and commits its correlated observation.
    #[allow(
        clippy::result_large_err,
        reason = "The error retains the correlated observation inline for retry."
    )]
    pub async fn execute(
        &mut self,
        mut session: SessionId,
    ) -> Result<
        ModelCallExecutionOutcome,
        ModelCallExecutionError<
            Prepare::Error,
            Failure::Error,
            Authorization::Error,
            Provider::Error,
            Observation::Error,
        >,
    > {
        if let Some(retained) = self.retained_state.take() {
            match retained.state {
                RetainedModelCallExecutionStateKind::PreparedFailure {
                    session,
                    turn,
                    call,
                    cause,
                    attachment_failure,
                } => match self
                    .failure
                    .reread_failure(session, call, attachment_failure)
                    .await
                {
                    Ok(RetainedPreparedFailureStatus::Pending) => {
                        return self
                            .commit_prepared_failure(session, turn, call, cause, attachment_failure)
                            .await;
                    }
                    Ok(RetainedPreparedFailureStatus::AlreadyCommitted) => {
                        report_turn_terminalization(
                            session,
                            turn,
                            TurnTerminalOutcome::from(cause),
                        );
                        return Ok(match cause {
                            PreparedModelCallFailureCause::CapabilityKnownFailure => {
                                ModelCallExecutionOutcome::CapabilityFailureAlreadyCommitted(call)
                            }
                            PreparedModelCallFailureCause::ToolRoundLimitReached => {
                                ModelCallExecutionOutcome::ToolRoundLimitAlreadyCommitted(call)
                            }
                        });
                    }
                    Ok(RetainedPreparedFailureStatus::Cancelled) => {
                        return Ok(ModelCallExecutionOutcome::NoWork);
                    }
                    Err(error) => {
                        self.retained_state = Some(RetainedModelCallExecutionState {
                            state: RetainedModelCallExecutionStateKind::PreparedFailure {
                                session,
                                turn,
                                call,
                                cause,
                                attachment_failure,
                            },
                        });
                        return Err(ModelCallExecutionError::PreparedFailureReread(error));
                    }
                },
                RetainedModelCallExecutionStateKind::AuthorizationNonConsumption {
                    session: retained_session,
                    prepared,
                } => match self
                    .authorization
                    .reread_after_ambiguous_commit(retained_session, &prepared)
                    .await
                {
                    Ok(ModelCallAuthorizationReread::Prepared) => {
                        session = retained_session;
                    }
                    Ok(ModelCallAuthorizationReread::InFlight(authorized)) => {
                        let non_consumption = authorized
                            .observation_correlation()
                            .bind_terminal_observation(ModelCallTerminalObservation::KnownFailed);
                        return self
                            .commit_terminal_observation(
                                retained_session,
                                non_consumption,
                                Box::new([]),
                            )
                            .await;
                    }
                    Ok(ModelCallAuthorizationReread::CancellationRequested(stopped)) => {
                        let cancellation = stopped
                            .observation_correlation()
                            .bind_terminal_observation(ModelCallTerminalObservation::Cancelled);
                        return self
                            .commit_terminal_observation(
                                retained_session,
                                cancellation,
                                Box::new([]),
                            )
                            .await;
                    }
                    Ok(ModelCallAuthorizationReread::Cancelled) => {
                        return Ok(ModelCallExecutionOutcome::NoWork);
                    }
                    Err(error) => {
                        self.retained_state = Some(RetainedModelCallExecutionState {
                            state:
                                RetainedModelCallExecutionStateKind::AuthorizationNonConsumption {
                                    session: retained_session,
                                    prepared,
                                },
                        });
                        return Err(ModelCallExecutionError::AuthorizationReconciliation(error));
                    }
                },
                RetainedModelCallExecutionStateKind::TerminalObservation {
                    session: retained_session,
                    observation: retained,
                    tool_approvals,
                } => match self
                    .observation
                    .reread_observation(retained_session, &retained)
                    .await
                {
                    Ok(RetainedModelCallObservationStatus::AlreadyCommitted) => {
                        return Ok(ModelCallExecutionOutcome::ObservationAlreadyCommitted(
                            retained.call(),
                        ));
                    }
                    Ok(RetainedModelCallObservationStatus::AvailabilitySuccessorCommitted {
                        retry_backoff,
                    }) => {
                        // The commit landed with its successor, so the turn is
                        // active on a new attempt rather than terminal. Waiting
                        // out the remaining delay returns the caller to ordinary
                        // preparation, which owns the successor from here.
                        return Ok(ModelCallExecutionOutcome::RetryBackoff(retry_backoff));
                    }
                    Ok(RetainedModelCallObservationStatus::Pending) => {
                        return self
                            .commit_terminal_observation(
                                retained_session,
                                *retained,
                                tool_approvals,
                            )
                            .await;
                    }
                    Ok(RetainedModelCallObservationStatus::DiscardedByLogicalTerminal) => {
                        return Ok(ModelCallExecutionOutcome::NoWork);
                    }
                    Err(error) => {
                        self.retained_state = Some(RetainedModelCallExecutionState {
                            state: RetainedModelCallExecutionStateKind::TerminalObservation {
                                session: retained_session,
                                observation: retained.clone(),
                                tool_approvals,
                            },
                        });
                        return Err(ModelCallExecutionError::ObservationCommit {
                            error,
                            retained_observation: *retained,
                        });
                    }
                },
            }
        }

        let prepared = loop {
            let call = self.ids.next_model_call_id();
            let failure_identities = self.next_failed_identities();
            let steering_frontier = self.ids.next_context_frontier_id();
            let prepare = &mut self.prepare;
            let ids = &mut self.ids;
            match prepare
                .prepare(session, call, failure_identities, steering_frontier, |_| {
                    (ids.next_semantic_entry_id(), ids.next_turn_id())
                })
                .await
            {
                Ok(PrepareModelCallOutcome::NoWork) => {
                    return Ok(ModelCallExecutionOutcome::NoWork);
                }
                Ok(PrepareModelCallOutcome::RetryBackoff(delay)) => {
                    return Ok(ModelCallExecutionOutcome::RetryBackoff(delay));
                }
                Ok(PrepareModelCallOutcome::PoolExhausted(exhausted)) => {
                    report_turn_terminalization(
                        exhausted.failed().session(),
                        exhausted.failed().turn(),
                        TurnTerminalOutcome::Failed,
                    );
                    return Ok(ModelCallExecutionOutcome::PoolExhausted(Box::new(
                        CredentialPoolExhaustedOutcome::BeforeCall(exhausted),
                    )));
                }
                Ok(PrepareModelCallOutcome::Checkpointed(call)) => {
                    return Ok(ModelCallExecutionOutcome::Checkpointed(call));
                }
                Ok(PrepareModelCallOutcome::Ready {
                    request,
                    credential_reference,
                    dangerous_tool_auto_approval,
                    recorded_user_overrides,
                    system_prompt,
                    tool_entries,
                    reasoning_provenance,
                }) => {
                    break (
                        request,
                        credential_reference,
                        dangerous_tool_auto_approval,
                        recorded_user_overrides,
                        system_prompt,
                        tool_entries,
                        reasoning_provenance,
                    );
                }
                Ok(PrepareModelCallOutcome::TargetUnavailable(failed)) => {
                    report_turn_terminalization(
                        failed.session(),
                        failed.turn(),
                        TurnTerminalOutcome::TargetUnavailable,
                    );
                    return Ok(ModelCallExecutionOutcome::TargetUnavailable(failed));
                }
                Err(error)
                    if error.operator_failure_class()
                        == OperatorFailureClass::IdentityCollision =>
                {
                    continue;
                }
                Err(error) => return Err(ModelCallExecutionError::Prepare(error)),
            }
        };

        let (
            prepared,
            credential_reference,
            dangerous_tool_auto_approval,
            recorded_user_overrides,
            system_prompt,
            tool_entries,
            reasoning_provenance,
        ) = prepared;
        let call = prepared.call().id();
        let attempt = prepared.attempt();
        let turn = prepared.turn();
        let prepared_request = (*prepared).clone();
        let advertised_tools = self.catalog.definitions();
        let operation = match PreparedModelOperation::render_within(
            *prepared,
            credential_reference,
            system_prompt,
            advertised_tools.clone(),
            &tool_entries,
            &reasoning_provenance,
            self.retained_frontier_content_limit,
        ) {
            Ok(operation) => operation,
            // The retained-content ceiling is a safety bound on the same
            // automatic tool loop the round ceiling bounds, so it closes the
            // checkpoint through the same terminal contract rather than
            // surfacing as an operator failure. Refusing here, before the
            // messages exist, is what keeps the closure reachable at all.
            Err(ModelFrontierRenderingError::RetainedFrontierContentLimitExceeded {
                observed_bytes,
                limit_bytes,
            }) => {
                tracing::warn!(
                    session_id = %session.as_uuid(),
                    turn_id = %turn.as_uuid(),
                    model_call_id = %call.into_uuid(),
                    retained_frontier_content_limit = limit_bytes,
                    observed_retained_frontier_content_bytes = observed_bytes,
                    "retained frontier content limit reached"
                );
                return self
                    .commit_prepared_failure(
                        session,
                        turn,
                        call,
                        PreparedModelCallFailureCause::ToolRoundLimitReached,
                        None,
                    )
                    .await;
            }
            Err(error) => return Err(ModelCallExecutionError::Render(error)),
        };
        // A deployment that configures no automatic tool-round ceiling leaves the
        // loop bounded by the retained-content ceiling above and by the turn's
        // own liveness watchdogs, so an absent limit admits the round rather than
        // substituting one the operator did not ask for.
        let observed_tool_rounds = automatic_tool_round_count(turn, operation.messages());
        if let Some(tool_round_limit) = self.max_automatic_tool_rounds_per_turn
            && observed_tool_rounds >= tool_round_limit
        {
            tracing::warn!(
                session_id = %session.as_uuid(),
                turn_id = %turn.as_uuid(),
                model_call_id = %call.into_uuid(),
                tool_round_limit,
                observed_tool_rounds,
                "automatic tool-round limit reached"
            );
            return self
                .commit_prepared_failure(
                    session,
                    turn,
                    call,
                    PreparedModelCallFailureCause::ToolRoundLimitReached,
                    None,
                )
                .await;
        }
        let preparation_cancellation = self.authorization.cancellation_signal(session, call);
        let capability = match self
            .provider
            .prepare_capability(operation, preparation_cancellation)
            .await
        {
            Ok(ModelCallCapabilityPreparation::Ready(capability)) => capability,
            Ok(ModelCallCapabilityPreparation::Cancelled) => {
                return Ok(ModelCallExecutionOutcome::NoWork);
            }
            Ok(ModelCallCapabilityPreparation::KnownFailure) => {
                return self
                    .commit_prepared_failure(
                        session,
                        turn,
                        call,
                        PreparedModelCallFailureCause::CapabilityKnownFailure,
                        None,
                    )
                    .await;
            }
            Ok(ModelCallCapabilityPreparation::AttachmentFailure(
                AttachmentPreparationFailure::Unavailable,
            )) => {
                return Ok(ModelCallExecutionOutcome::AttachmentUnavailable);
            }
            Ok(ModelCallCapabilityPreparation::AttachmentFailure(
                failure @ (AttachmentPreparationFailure::TooLarge { .. }
                | AttachmentPreparationFailure::Missing
                | AttachmentPreparationFailure::Corrupt),
            )) => {
                return self
                    .commit_prepared_failure(
                        session,
                        turn,
                        call,
                        PreparedModelCallFailureCause::CapabilityKnownFailure,
                        Some(failure),
                    )
                    .await;
            }
            Err(error) => {
                return Err(ModelCallExecutionError::CapabilityPreparation(error));
            }
        };

        let permit = self.gate.acquire(attempt).await;
        let authorized = match self.authorization.authorize(session, call).await {
            Ok(AuthorizeModelCallOutcome::NoSend) => {
                drop(capability);
                drop(permit);
                return Ok(ModelCallExecutionOutcome::NoWork);
            }
            Ok(AuthorizeModelCallOutcome::Authorized(authorized)) => *authorized,
            Err(error)
                if matches!(
                    error.operator_failure_class(),
                    OperatorFailureClass::Infrastructure {
                        commit_ambiguous: true
                    }
                ) =>
            {
                match self
                    .authorization
                    .reread_after_ambiguous_commit(session, &prepared_request)
                    .await
                {
                    Ok(ModelCallAuthorizationReread::Prepared) => {
                        drop(capability);
                        drop(permit);
                        return Err(ModelCallExecutionError::Authorization(error));
                    }
                    Ok(ModelCallAuthorizationReread::InFlight(authorized)) => {
                        drop(capability);
                        drop(permit);
                        let non_consumption = authorized
                            .observation_correlation()
                            .bind_terminal_observation(ModelCallTerminalObservation::KnownFailed);
                        return self
                            .commit_terminal_observation(session, non_consumption, Box::new([]))
                            .await;
                    }
                    Ok(ModelCallAuthorizationReread::CancellationRequested(stopped)) => {
                        drop(capability);
                        drop(permit);
                        let cancellation = stopped
                            .observation_correlation()
                            .bind_terminal_observation(ModelCallTerminalObservation::Cancelled);
                        return self
                            .commit_terminal_observation(session, cancellation, Box::new([]))
                            .await;
                    }
                    Ok(ModelCallAuthorizationReread::Cancelled) => {
                        drop(capability);
                        drop(permit);
                        return Ok(ModelCallExecutionOutcome::NoWork);
                    }
                    Err(reread_error) => {
                        drop(capability);
                        drop(permit);
                        self.retained_state = Some(RetainedModelCallExecutionState {
                            state:
                                RetainedModelCallExecutionStateKind::AuthorizationNonConsumption {
                                    session,
                                    prepared: Box::new(prepared_request),
                                },
                        });
                        return Err(ModelCallExecutionError::AuthorizationReread {
                            authorization_error: error,
                            reread_error,
                        });
                    }
                }
            }
            Err(error) => return Err(ModelCallExecutionError::Authorization(error)),
        };
        let acceptance_possible = move || drop(permit);
        let invocation_cancellation = self.authorization.cancellation_signal(session, call);
        let observation = self
            .provider
            .invoke(
                authorized,
                capability,
                acceptance_possible,
                invocation_cancellation,
            )
            .await;
        let observation = observation.map_err(ModelCallExecutionError::Provider)?;

        let tool_approvals = self.tool_approvals(
            observation.observation(),
            dangerous_tool_auto_approval,
            &advertised_tools,
            &recorded_user_overrides,
        );
        self.commit_terminal_observation(session, observation, tool_approvals)
            .await
    }

    #[allow(
        clippy::result_large_err,
        reason = "The error retains the correlated observation inline for retry."
    )]
    async fn commit_prepared_failure(
        &mut self,
        session: SessionId,
        turn: TurnId,
        call: ModelCallId,
        cause: PreparedModelCallFailureCause,
        attachment_failure: Option<AttachmentPreparationFailure>,
    ) -> Result<
        ModelCallExecutionOutcome,
        ModelCallExecutionError<
            Prepare::Error,
            Failure::Error,
            Authorization::Error,
            Provider::Error,
            Observation::Error,
        >,
    > {
        loop {
            let identities = self.next_failed_identities();
            let ids = &mut self.ids;
            let next_turn = move |_| ids.next_turn_id();
            match self
                .failure
                .fail_prepared(
                    session,
                    call,
                    cause,
                    attachment_failure,
                    identities,
                    next_turn,
                )
                .await
            {
                Ok(failed) => {
                    let terminal_outcome = TurnTerminalOutcome::from(cause);
                    report_turn_terminalization(failed.session(), failed.turn(), terminal_outcome);
                    return Ok(match cause {
                        PreparedModelCallFailureCause::CapabilityKnownFailure => {
                            ModelCallExecutionOutcome::CapabilityKnownFailure(Box::new(failed))
                        }
                        PreparedModelCallFailureCause::ToolRoundLimitReached => {
                            ModelCallExecutionOutcome::ToolRoundLimitReached(Box::new(failed))
                        }
                    });
                }
                Err(error)
                    if error.operator_failure_class()
                        == OperatorFailureClass::IdentityCollision =>
                {
                    continue;
                }
                Err(error) => {
                    self.retained_state = Some(RetainedModelCallExecutionState {
                        state: RetainedModelCallExecutionStateKind::PreparedFailure {
                            session,
                            turn,
                            call,
                            cause,
                            attachment_failure,
                        },
                    });
                    return Err(ModelCallExecutionError::PreparedFailureCommit(error));
                }
            }
        }
    }

    #[allow(
        clippy::result_large_err,
        reason = "The error retains the correlated observation inline for retry."
    )]
    async fn commit_terminal_observation(
        &mut self,
        session: SessionId,
        observation: CorrelatedModelCallTerminalObservation,
        tool_approvals: Box<[InitialToolApproval]>,
    ) -> Result<
        ModelCallExecutionOutcome,
        ModelCallExecutionError<
            Prepare::Error,
            Failure::Error,
            Authorization::Error,
            Provider::Error,
            Observation::Error,
        >,
    > {
        loop {
            let mut identities =
                self.next_terminal_identities(observation.observation(), &tool_approvals);
            // Every classified pool trigger evaluates its frozen action, not
            // only the ones that could substitute a member on this turn.
            // `switch_next_turn`, `avoid_new_sessions`, and `quarantine`
            // terminalize the call and persist a durable exclusion, so gating
            // them on substitution proof silently degraded them to `stay`.
            // Persistence still requires the proof before creating a successor.
            if matches!(
                observation.provider_failure_cause(),
                Some(
                    signalbox_domain::ProviderModelCallFailureCause::RateLimited
                        | signalbox_domain::ProviderModelCallFailureCause::QuotaExhausted
                        | signalbox_domain::ProviderModelCallFailureCause::Overloaded
                        | signalbox_domain::ProviderModelCallFailureCause::ProviderInternal
                        | signalbox_domain::ProviderModelCallFailureCause::CredentialRejected
                )
            ) && let ModelCallTerminalIdentityCandidates::Exact(
                signalbox_domain::ModelCallTerminalIdentities::Failed(failed),
            ) = identities
            {
                identities = ModelCallTerminalIdentityCandidates::Availability {
                    failed,
                    successor_attempt: self.ids.next_turn_attempt_id(),
                };
            }
            let ids = &mut self.ids;
            let next_turn = move |_| ids.next_turn_id();
            match self
                .observation
                .commit_observation(session, observation.clone(), identities, next_turn)
                .await
            {
                Ok(Some(ModelCallObservationCommitOutcome::Terminal(outcome))) => {
                    report_model_call_terminalization(&outcome);
                    return Ok(ModelCallExecutionOutcome::ObservationCommitted(outcome));
                }
                Ok(Some(ModelCallObservationCommitOutcome::AvailabilitySuccessor(successor))) => {
                    return Ok(ModelCallExecutionOutcome::AvailabilitySuccessor(successor));
                }
                Ok(Some(ModelCallObservationCommitOutcome::PoolExhausted(exhausted))) => {
                    if let CredentialPoolExhaustedOutcome::AfterCall { terminal, .. } = &exhausted {
                        report_model_call_terminalization(terminal);
                    }
                    return Ok(ModelCallExecutionOutcome::PoolExhausted(Box::new(
                        exhausted,
                    )));
                }
                Ok(None) => return Ok(ModelCallExecutionOutcome::NoWork),
                Err(error)
                    if error.operator_failure_class()
                        == OperatorFailureClass::IdentityCollision =>
                {
                    continue;
                }
                Err(error) => {
                    self.retained_state = Some(RetainedModelCallExecutionState {
                        state: RetainedModelCallExecutionStateKind::TerminalObservation {
                            session,
                            observation: Box::new(observation.clone()),
                            tool_approvals,
                        },
                    });
                    return Err(ModelCallExecutionError::ObservationCommit {
                        error,
                        retained_observation: observation,
                    });
                }
            }
        }
    }

    fn next_failed_identities(&mut self) -> FailedModelCallTurnIdentities {
        FailedModelCallTurnIdentities::new(
            self.ids.next_semantic_entry_id(),
            self.ids.next_context_frontier_id(),
        )
    }

    fn next_terminal_identities(
        &mut self,
        observation: &ModelCallTerminalObservation,
        tool_approvals: &[InitialToolApproval],
    ) -> ModelCallTerminalIdentityCandidates {
        let exact = match observation {
            ModelCallTerminalObservation::Completed { assistant_text } => {
                let assistant_entries = (0..assistant_text.len())
                    .map(|_| self.ids.next_semantic_entry_id())
                    .collect();
                ModelCallTerminalIdentities::Completed(CompletedModelCallIdentities::new(
                    assistant_entries,
                    self.ids.next_semantic_entry_id(),
                    self.ids.next_context_frontier_id(),
                ))
            }
            ModelCallTerminalObservation::CompletedWithProviderCompaction { response, .. }
            | ModelCallTerminalObservation::CompletedWithProviderReasoning { response } => {
                let assistant_entries = (0..response.len())
                    .map(|_| self.ids.next_semantic_entry_id())
                    .collect();
                ModelCallTerminalIdentities::Completed(CompletedModelCallIdentities::new(
                    assistant_entries,
                    self.ids.next_semantic_entry_id(),
                    self.ids.next_context_frontier_id(),
                ))
            }
            ModelCallTerminalObservation::CompletedWithTools { response, .. } => {
                let mut approval_index = 0usize;
                let mut continuing = Vec::with_capacity(response.parts().len());
                let mut stopped = Vec::with_capacity(response.parts().len());
                let mut every_request_approved = true;
                for part in response.parts() {
                    match part {
                        AssistantResponsePart::Text(_) => {
                            continuing.push(ToolResponsePartIdentity::text(
                                self.ids.next_semantic_entry_id(),
                            ));
                        }
                        AssistantResponsePart::ProviderCompaction(_) => {
                            continuing.push(ToolResponsePartIdentity::provider_compaction(
                                self.ids.next_semantic_entry_id(),
                            ));
                        }
                        AssistantResponsePart::ProviderReasoning(_) => {
                            continuing.push(ToolResponsePartIdentity::provider_reasoning(
                                self.ids.next_semantic_entry_id(),
                            ));
                        }
                        AssistantResponsePart::ToolCall(_) => {
                            // A retained-policy count mismatch is an internal
                            // defect. Confirm is the conservative candidate:
                            // it cannot grant unattended execution, and the
                            // domain still rejects it under blanket posture.
                            let approval = tool_approvals
                                .get(approval_index)
                                .copied()
                                .unwrap_or(InitialToolApproval::Confirm);
                            approval_index += 1;
                            every_request_approved &= !approval.requires_decision();
                            continuing.push(ToolResponsePartIdentity::tool_call(
                                self.ids.next_semantic_entry_id(),
                                self.ids.next_tool_request_id(),
                                approval,
                            ));
                        }
                    }
                }
                debug_assert_eq!(approval_index, tool_approvals.len());
                let continuation_attempt =
                    every_request_approved.then(|| self.ids.next_turn_attempt_id());
                let mut stopped_approval_index = 0usize;
                for part in response.parts() {
                    match part {
                        AssistantResponsePart::Text(_) => {
                            stopped.push(StoppedToolResponsePartIdentity::text(
                                self.ids.next_semantic_entry_id(),
                            ));
                        }
                        AssistantResponsePart::ProviderCompaction(_) => {
                            stopped.push(StoppedToolResponsePartIdentity::provider_compaction(
                                self.ids.next_semantic_entry_id(),
                            ));
                        }
                        AssistantResponsePart::ProviderReasoning(_) => {
                            stopped.push(StoppedToolResponsePartIdentity::provider_reasoning(
                                self.ids.next_semantic_entry_id(),
                            ));
                        }
                        AssistantResponsePart::ToolCall(_) => {
                            let approval = tool_approvals
                                .get(stopped_approval_index)
                                .copied()
                                .unwrap_or(InitialToolApproval::Confirm);
                            stopped_approval_index += 1;
                            stopped.push(StoppedToolResponsePartIdentity::tool_call(
                                self.ids.next_semantic_entry_id(),
                                self.ids.next_tool_request_id(),
                                self.ids.next_semantic_entry_id(),
                                approval,
                            ));
                        }
                    }
                }
                debug_assert_eq!(stopped_approval_index, tool_approvals.len());
                return ModelCallTerminalIdentityCandidates::ToolRound {
                    continuing: ToolRoundModelCallIdentities::new(
                        continuing,
                        self.ids.next_context_frontier_id(),
                        continuation_attempt,
                    ),
                    stopped: StoppedToolRoundModelCallIdentities::new(
                        stopped,
                        self.ids.next_semantic_entry_id(),
                        self.ids.next_context_frontier_id(),
                    ),
                };
            }
            ModelCallTerminalObservation::KnownFailed => {
                ModelCallTerminalIdentities::Failed(self.next_failed_identities())
            }
            ModelCallTerminalObservation::Cancelled => {
                ModelCallTerminalIdentities::PhysicalCancellation(
                    PhysicalCancellationModelCallTurnIdentities::new(
                        self.ids.next_semantic_entry_id(),
                        self.ids.next_context_frontier_id(),
                    ),
                )
            }
            ModelCallTerminalObservation::Refused => ModelCallTerminalIdentities::Refused(
                RefusedModelCallTurnIdentities::new(self.ids.next_context_frontier_id()),
            ),
            ModelCallTerminalObservation::RefusedWithProviderCompaction {
                provider_compaction,
                ..
            } => ModelCallTerminalIdentities::Refused(
                RefusedModelCallTurnIdentities::new(self.ids.next_context_frontier_id())
                    .with_provider_compaction_entries(
                        (0..provider_compaction.len())
                            .map(|_| self.ids.next_semantic_entry_id())
                            .collect(),
                    ),
            ),
            ModelCallTerminalObservation::Ambiguous => ModelCallTerminalIdentities::Ambiguous(
                AmbiguousModelCallTurnIdentities::new(self.ids.next_context_frontier_id()),
            ),
        };
        ModelCallTerminalIdentityCandidates::Exact(exact)
    }

    /// Selects one initial approval per proposal, consuming recorded user
    /// overrides.
    ///
    /// An recorded override substitutes for the judge only where the judge would
    /// otherwise decide: the base selection must be `Delegated`, and the
    /// proposal must re-propose the exact denied command. Each recorded override
    /// is consumed at most once per response — a second identical proposal
    /// parks for the judge again — mirroring the one-shot uniqueness the
    /// decision table enforces durably.
    fn tool_approvals(
        &self,
        observation: &ModelCallTerminalObservation,
        posture: DangerousToolAutoApproval,
        advertised_tools: &[ToolDefinition],
        recorded_user_overrides: &[RecordedUserOverride],
    ) -> Box<[InitialToolApproval]> {
        let ModelCallTerminalObservation::CompletedWithTools { response, .. } = observation else {
            return Box::new([]);
        };
        let mut remaining_overrides: Vec<&RecordedUserOverride> =
            recorded_user_overrides.iter().collect();
        response
            .parts()
            .iter()
            .filter_map(|part| match part {
                AssistantResponsePart::Text(_)
                | AssistantResponsePart::ProviderCompaction(_)
                | AssistantResponsePart::ProviderReasoning(_) => None,
                AssistantResponsePart::ToolCall(proposal) => {
                    if proposal.is_suppressed() {
                        return Some(InitialToolApproval::RuntimeSafetyDeny);
                    }
                    let definition = advertised_tools
                        .iter()
                        .find(|definition| definition.name() == proposal.name());
                    let base = initial_tool_approval(posture, definition);
                    if base != InitialToolApproval::Delegated {
                        return Some(base);
                    }
                    let matched = remaining_overrides
                        .iter()
                        .position(|recorded| recorded.matches_proposal(proposal));
                    Some(match matched {
                        Some(index) => {
                            let recorded = remaining_overrides.remove(index);
                            InitialToolApproval::UserOverride {
                                command: recorded.command(),
                                denied_request: recorded.denied_request(),
                            }
                        }
                        None => base,
                    })
                }
            })
            .collect()
    }
}

/// One durable result of committing a correlated model-call observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelCallObservationCommitOutcome {
    /// The observation reached an ordinary terminal or durable-wait outcome.
    Terminal(Box<ModelCallTerminalOutcome>),
    /// Pool policy authorized a distinct availability successor attempt.
    AvailabilitySuccessor(Box<AvailabilitySuccessorOutcome>),
    /// Every member is unavailable; the pool, not one member, terminalized.
    PoolExhausted(CredentialPoolExhaustedOutcome),
}

/// Typed pool-wide terminal cause, distinct from one account's failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CredentialPoolExhaustedOutcome {
    /// Selection found no member before creating a call.
    BeforeCall(Box<CredentialPoolExhaustedModelCallTurn>),
    /// A qualifying member failure consumed the last available member.
    AfterCall {
        /// Deployment-owned pool name.
        pool_name: Arc<str>,
        /// Ordinary terminal projection retaining the last call's evidence.
        terminal: Box<ModelCallTerminalOutcome>,
    },
}

/// One committed availability successor and its capped retry delay.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AvailabilitySuccessorOutcome {
    successor: AvailabilitySuccessorModelCallTurn,
    backoff: Duration,
}

impl AvailabilitySuccessorOutcome {
    /// Creates the application result after persistence freezes the deadline.
    pub const fn new(successor: AvailabilitySuccessorModelCallTurn, backoff: Duration) -> Self {
        Self { successor, backoff }
    }

    /// Borrows the exact predecessor/successor lifecycle transition.
    pub const fn successor(&self) -> &AvailabilitySuccessorModelCallTurn {
        &self.successor
    }

    /// Returns the capped delay frozen with the durable successor.
    pub const fn backoff(&self) -> Duration {
        self.backoff
    }
}

/// Closed terminal labels admitted to the turn lifecycle event.
///
/// Callers select a typed variant from their exhaustive domain outcome instead
/// of supplying a positional string that could drift from committed state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TurnTerminalOutcome {
    Completed,
    CancelledWithToolResponse,
    Failed,
    Cancelled,
    Refused,
    TargetUnavailable,
    CapabilityKnownFailure,
    ToolRoundLimitReached,
}

impl TurnTerminalOutcome {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::CancelledWithToolResponse => "cancelled_with_tool_response",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Refused => "refused",
            Self::TargetUnavailable => "target_unavailable",
            Self::CapabilityKnownFailure => "capability_known_failure",
            Self::ToolRoundLimitReached => "tool_round_limit_reached",
        }
    }
}

impl From<PreparedModelCallFailureCause> for TurnTerminalOutcome {
    fn from(cause: PreparedModelCallFailureCause) -> Self {
        match cause {
            PreparedModelCallFailureCause::CapabilityKnownFailure => Self::CapabilityKnownFailure,
            PreparedModelCallFailureCause::ToolRoundLimitReached => Self::ToolRoundLimitReached,
        }
    }
}

/// Records terminal model-call commits while excluding nonterminal waits.
///
/// Each arm is exhaustive over the domain-owned outcome, keeping the label
/// derived from the committed state rather than supplied independently.
fn report_model_call_terminalization(outcome: &ModelCallTerminalOutcome) {
    let (session, turn, terminal_outcome) = match outcome {
        ModelCallTerminalOutcome::Completed(value) => (
            value.session(),
            value.turn(),
            TurnTerminalOutcome::Completed,
        ),
        ModelCallTerminalOutcome::CancelledWithToolResponse(value) => (
            value.session(),
            value.turn(),
            TurnTerminalOutcome::CancelledWithToolResponse,
        ),
        ModelCallTerminalOutcome::Failed(value) => {
            (value.session(), value.turn(), TurnTerminalOutcome::Failed)
        }
        ModelCallTerminalOutcome::Cancelled(value) => (
            value.session(),
            value.turn(),
            TurnTerminalOutcome::Cancelled,
        ),
        ModelCallTerminalOutcome::Refused(value) => {
            (value.session(), value.turn(), TurnTerminalOutcome::Refused)
        }
        ModelCallTerminalOutcome::ReconciliationRequired(value) => {
            report_turn_parked_for_reconciliation(value.session(), value.turn());
            return;
        }
        ModelCallTerminalOutcome::ToolRound(_) | ModelCallTerminalOutcome::AwaitingRecovery(_) => {
            return;
        }
    };
    report_turn_terminalization(session, turn, terminal_outcome);
}

/// Emits one content-free record for a turn parked on user reconciliation.
///
/// Session and turn are daemon-minted identities, while the event name is a
/// closed lifecycle state. Ambiguity details and model content remain absent.
fn report_turn_parked_for_reconciliation(session: SessionId, turn: TurnId) {
    tracing::warn!(
        session_id = %session.into_uuid(),
        turn_id = %turn.into_uuid(),
        "turn parked awaiting bounded reconciliation"
    );
}

/// Emits one content-free terminal lifecycle record for an operator.
///
/// Session, turn, and the closed outcome token are sufficient to distinguish
/// completed work from an active or parked daemon without exposing payloads.
fn report_turn_terminalization(
    session: SessionId,
    turn: TurnId,
    terminal_outcome: TurnTerminalOutcome,
) {
    tracing::info!(
        session_id = %session.as_uuid(),
        turn_id = %turn.as_uuid(),
        terminal_outcome = terminal_outcome.as_str(),
        "turn terminalized"
    );
}
/// Counts one turn's distinct automatic tool rounds in a rendered frontier.
///
/// The count is the quantity a deployment's configured ceiling is compared
/// against; the comparison itself stays at the checkpoint that owns the
/// configured limit.
fn automatic_tool_round_count(turn: TurnId, messages: &[ModelConversationMessage]) -> usize {
    messages
        .iter()
        .filter_map(|message| match message {
            ModelConversationMessage::AssistantToolUse {
                producing_call,
                request,
                ..
            } if request.turn() == turn => Some(*producing_call),
            _ => None,
        })
        .collect::<BTreeSet<_>>()
        .len()
}

/// One deterministic scripted-provider action.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ScriptedModelCallStep {
    /// Capability preparation returns a trustworthy ordinary failure.
    CapabilityKnownFailure,
    /// Capability preparation observes durable cancellation.
    CapabilityCancelled,
    /// Capability preparation reports an operator failure.
    CapabilityOperatorFailure,
    /// Capability succeeds but provider interaction reports no observation.
    InteractionOperatorFailure,
    /// Provider interaction returns this exact terminal observation.
    Return(ModelCallTerminalObservation),
}

#[derive(signalbox_derive::OperatorError)]
/// Sanitized failure from the deterministic scripted provider.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScriptedModelCallError {
    #[error("scripted model-call actions are exhausted")]
    /// No scripted action remained for a requested capability.
    ScriptExhausted,
    #[error("scripted model-call capability preparation failed")]
    /// The script explicitly selected a capability-stage operator failure.
    CapabilityOperatorFailure,
    #[error("scripted model-call interaction failed")]
    /// The script explicitly selected an interaction-stage operator failure.
    InteractionOperatorFailure,
    #[error("scripted model-call authorization does not match its capability")]
    /// Issued authorization did not match the prepared capability.
    AuthorizationMismatch,
}

impl ClassifyOperatorFailure for ScriptedModelCallError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        OperatorFailureClass::CallerOrHubBug
    }
}

/// Opaque one-shot capability owned by [`ScriptedModelCallProvider`].
pub struct ScriptedModelCallCapability {
    operation: PreparedModelOperation,
    step: ScriptedModelCallStep,
}

/// Deterministic in-repository implementation of the provider port.
#[derive(Debug)]
pub struct ScriptedModelCallProvider {
    steps: std::collections::VecDeque<ScriptedModelCallStep>,
    capability_preparation_count: usize,
    interaction_count: usize,
    last_prepared_messages: Option<Box<[ModelConversationMessage]>>,
    last_prepared_tools: Option<Box<[ToolDefinition]>>,
    last_prepared_system_prompt: Option<Option<String>>,
}

impl ScriptedModelCallProvider {
    /// Creates a provider that consumes actions in supplied order.
    ///
    /// Capability-stage actions are consumed during preparation. Interaction
    /// actions remain queued until their prepared capability is invoked, so a
    /// proven authorization rollback can prepare the same action again.
    pub fn new(steps: impl IntoIterator<Item = ScriptedModelCallStep>) -> Self {
        Self {
            steps: steps.into_iter().collect(),
            capability_preparation_count: 0,
            interaction_count: 0,
            last_prepared_messages: None,
            last_prepared_tools: None,
            last_prepared_system_prompt: None,
        }
    }

    /// Returns how many capability-preparation calls occurred.
    pub const fn capability_preparation_count(&self) -> usize {
        self.capability_preparation_count
    }

    /// Returns how many physical interaction calls occurred.
    pub const fn interaction_count(&self) -> usize {
        self.interaction_count
    }

    /// Returns how many scripted actions remain.
    pub fn remaining_step_count(&self) -> usize {
        self.steps.len()
    }

    /// Borrows the exact messages most recently presented for capability
    /// preparation.
    pub fn last_prepared_messages(&self) -> Option<&[ModelConversationMessage]> {
        self.last_prepared_messages.as_deref()
    }

    /// Borrows the exact catalog snapshot most recently presented for
    /// capability preparation.
    pub fn last_prepared_tools(&self) -> Option<&[ToolDefinition]> {
        self.last_prepared_tools.as_deref()
    }

    /// Borrows the exact optional system prompt most recently presented for
    /// capability preparation.
    pub fn last_prepared_system_prompt(&self) -> Option<Option<&str>> {
        self.last_prepared_system_prompt
            .as_ref()
            .map(|prompt| prompt.as_deref())
    }
}

impl ModelCallProvider for ScriptedModelCallProvider {
    type Capability = ScriptedModelCallCapability;
    type Error = ScriptedModelCallError;

    fn prepare_capability<Cancellation>(
        &mut self,
        operation: PreparedModelOperation,
        cancellation: Cancellation,
    ) -> impl Future<Output = Result<ModelCallCapabilityPreparation<Self::Capability>, Self::Error>> + Send
    where
        Cancellation: Future<Output = ()> + Send + 'static,
    {
        drop(cancellation);
        self.capability_preparation_count += 1;
        self.last_prepared_messages = Some(operation.messages().to_vec().into_boxed_slice());
        self.last_prepared_tools = Some(operation.tools().to_vec().into_boxed_slice());
        self.last_prepared_system_prompt = Some(operation.system_prompt().map(str::to_owned));
        let step = self.steps.front().cloned();
        if matches!(
            &step,
            Some(
                ScriptedModelCallStep::CapabilityKnownFailure
                    | ScriptedModelCallStep::CapabilityCancelled
                    | ScriptedModelCallStep::CapabilityOperatorFailure
            )
        ) {
            self.steps.pop_front();
        }
        async move {
            match step.ok_or(ScriptedModelCallError::ScriptExhausted)? {
                ScriptedModelCallStep::CapabilityKnownFailure => {
                    Ok(ModelCallCapabilityPreparation::KnownFailure)
                }
                ScriptedModelCallStep::CapabilityCancelled => {
                    Ok(ModelCallCapabilityPreparation::Cancelled)
                }
                ScriptedModelCallStep::CapabilityOperatorFailure => {
                    Err(ScriptedModelCallError::CapabilityOperatorFailure)
                }
                step @ (ScriptedModelCallStep::InteractionOperatorFailure
                | ScriptedModelCallStep::Return(_)) => Ok(ModelCallCapabilityPreparation::Ready(
                    ScriptedModelCallCapability { operation, step },
                )),
            }
        }
    }

    fn invoke<AcceptancePossible, Cancellation>(
        &mut self,
        authorized: AuthorizedModelCall,
        capability: Self::Capability,
        acceptance_possible: AcceptancePossible,
        cancellation: Cancellation,
    ) -> impl Future<Output = Result<CorrelatedModelCallTerminalObservation, Self::Error>> + Send
    where
        AcceptancePossible: FnOnce() + Send,
        Cancellation: Future<Output = ()> + Send + 'static,
    {
        drop(cancellation);
        self.interaction_count += 1;
        let prepared = capability.operation.request();
        let step = if prepared.session() != authorized.session()
            || prepared.turn() != authorized.turn()
            || prepared.attempt() != authorized.attempt().id()
            || prepared.call().id() != authorized.call().id()
            || prepared.call().selection() != authorized.call().selection()
            || prepared.call().target() != authorized.call().target()
            || prepared.call().frontier() != authorized.call().frontier()
        {
            Err(ScriptedModelCallError::AuthorizationMismatch)
        } else {
            match self.steps.front() {
                None => Err(ScriptedModelCallError::ScriptExhausted),
                Some(step) if step != &capability.step => {
                    Err(ScriptedModelCallError::AuthorizationMismatch)
                }
                Some(_) => self
                    .steps
                    .pop_front()
                    .ok_or(ScriptedModelCallError::ScriptExhausted),
            }
        };
        async move {
            let step = step?;
            acceptance_possible();
            match step {
                ScriptedModelCallStep::Return(observation) => Ok(authorized
                    .observation_correlation()
                    .bind_terminal_observation(observation)),
                ScriptedModelCallStep::InteractionOperatorFailure => {
                    Err(ScriptedModelCallError::InteractionOperatorFailure)
                }
                ScriptedModelCallStep::CapabilityKnownFailure
                | ScriptedModelCallStep::CapabilityCancelled
                | ScriptedModelCallStep::CapabilityOperatorFailure => {
                    Err(ScriptedModelCallError::ScriptExhausted)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;

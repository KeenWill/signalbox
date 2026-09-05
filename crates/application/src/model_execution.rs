//! First text-only model-call execution orchestration.
//!
//! docs/spec/model-call-execution.md owns the staged transaction and
//! provider-effect order. The application keeps persistence, provider
//! capability preparation, send authorization, provider interaction, and
//! terminal observation distinct.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    error::Error,
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
// numeric-bound: guard - prevents retained frontier content from exhausting daemon memory as rounds multiply
const MAX_RETAINED_FRONTIER_CONTENT_BYTES: usize = 256 * 1024 * 1024;

// Worst-case compact JSON for maximum checked metadata, u64 length, and digest.
// numeric-bound: guard - prevents a stub the retained-content sum excludes from growing unbounded
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
    PhysicalCancellationModelCallTurnIdentities, PreparedModelCallRequest, RecordedUserOverride,
    RefusedModelCallTurnIdentities, SemanticTranscriptEntryId, SemanticTranscriptEntryPayload,
    SemanticTranscriptEntryRef, SessionConfigurationDefaultsVersion, SessionId,
    SessionSystemPrompt, StopRequestedModelCallTurn, StoppedToolResponsePartIdentity,
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

/// Application rendering of one semantic frontier entry as a provider message.
///
/// The source-qualified semantic entry, rather than a native turn assumption,
/// preserves the provenance of entries inherited across sessions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelConversationMessage {
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

fn render_frontier_messages<'a>(
    entries: impl IntoIterator<
        Item = (
            SemanticTranscriptEntryRef,
            &'a SemanticTranscriptEntryPayload,
        ),
    >,
    mut origin_content: impl FnMut(AcceptedInputId) -> Option<UserContent>,
    mut attachment_byte_length: impl FnMut(BlobDigest) -> Option<NonZeroU64>,
    tool_entries: impl IntoIterator<Item = &'a ResolvedToolConversationEntry>,
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
            // Identity-only payloads carry no content of their own. Tool
            // payloads name evidence rather than carrying it, and that
            // evidence is summed below.
            SemanticTranscriptEntryPayload::ModelIdentityChanged { .. }
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
    ) -> Result<Self, ModelFrontierRenderingError> {
        Self::render_within(
            request,
            credential_reference,
            system_prompt,
            tools,
            tool_entries,
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
        let messages = render_frontier_messages(
            projected_entries,
            |accepted_input| request.origin_content(accepted_input).cloned(),
            |digest| request.attachment_byte_length(digest),
            projected_tool_entries,
        )?;
        Ok(Self {
            request,
            credential_reference,
            system_prompt,
            messages,
            tools,
        })
    }

    /// Borrows the checked durable request facts.
    pub const fn request(&self) -> &PreparedModelCallRequest {
        &self.request
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

/// A checked frontier could not be projected into the current text-only input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelFrontierRenderingError {
    /// A frontier origin was missing its reconstituted accepted-input content.
    MissingOriginContent {
        /// The source-qualified origin entry.
        entry: SemanticTranscriptEntryRef,
        /// The accepted input whose content was absent.
        accepted_input: AcceptedInputId,
    },
    /// A referenced attachment lacked its immutable catalog length fact.
    MissingAttachmentBlobFact {
        /// Global blob identity whose catalog projection was absent.
        digest: BlobDigest,
    },
    /// Canonical attachment metadata could not be serialized.
    AttachmentStubSerialization,
    /// Checked attachment metadata exceeded its derived rendered bound.
    AttachmentStubBoundExceeded,
    /// Two storage evidence values claimed the same semantic entry.
    DuplicateToolEvidence {
        /// Duplicated source-qualified entry.
        entry: SemanticTranscriptEntryRef,
    },
    /// Reference-only tool history lacks exact correlated durable authority.
    MissingOrMismatchedToolEvidence {
        /// Source-qualified entry whose evidence is absent or cross-wired.
        entry: SemanticTranscriptEntryRef,
    },
    /// Durable ambiguity cannot be projected as an ordinary model-visible result.
    UnrenderableToolResult {
        /// Source-qualified result entry.
        entry: SemanticTranscriptEntryRef,
    },
    /// Storage supplied evidence not named by the checked frontier.
    UnexpectedToolEvidence {
        /// Extra source-qualified entry.
        entry: SemanticTranscriptEntryRef,
    },
    /// A projection named an entry absent from its complete source frontier.
    MissingProjectedEntry {
        /// The absent source-qualified entry.
        entry: SemanticTranscriptEntryRef,
    },
    /// A stored delegation wait mode contradicted its delivery position.
    InvalidDelegationDelivery {
        /// Source-qualified delegation-result entry.
        entry: SemanticTranscriptEntryRef,
    },
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
    /// The complete durable frontier carries malformed summary provenance.
    InvalidContextProjection(ContextFrontierProjectionFailure),
}

impl fmt::Display for ModelFrontierRenderingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingOriginContent { .. } => {
                formatter.write_str("model frontier origin content is missing")
            }
            Self::MissingAttachmentBlobFact { .. } => {
                formatter.write_str("model frontier attachment catalog fact is missing")
            }
            Self::AttachmentStubSerialization => {
                formatter.write_str("model frontier attachment stub could not be serialized")
            }
            Self::AttachmentStubBoundExceeded => {
                formatter.write_str("model frontier attachment stub exceeded its byte bound")
            }
            Self::DuplicateToolEvidence { .. } => {
                formatter.write_str("model frontier tool evidence is duplicated")
            }
            Self::MissingOrMismatchedToolEvidence { .. } => {
                formatter.write_str("model frontier tool evidence is missing or mismatched")
            }
            Self::UnrenderableToolResult { .. } => {
                formatter.write_str("model frontier contains an unrenderable tool result")
            }
            Self::UnexpectedToolEvidence { .. } => {
                formatter.write_str("model frontier tool evidence is not referenced")
            }
            Self::MissingProjectedEntry { .. } => {
                formatter.write_str("context projection entry is missing from its frontier")
            }
            Self::InvalidDelegationDelivery { .. } => {
                formatter.write_str("model frontier delegation delivery is inconsistent")
            }
            Self::RetainedFrontierContentLimitExceeded { .. } => {
                formatter.write_str("model frontier retained content exceeds its ceiling")
            }
            Self::InvalidContextProjection(_) => {
                formatter.write_str("invalid context-compaction projection")
            }
        }
    }
}

impl Error for ModelFrontierRenderingError {}

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
/// never occurred, or dropping an unchanged terminal observation.  and
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

/// Outcome of one exact provider-native prospective input count.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelCallInputTokenCount {
    /// Exact provider-reported count for the rendered operation.
    Counted(u64),
    /// Authority or caller cancellation won before a count completed.
    Cancelled,
}

/// Provider adapter boundary for exact prospective input counting.
pub trait ModelCallInputTokenCounter {
    /// Sanitized adapter-specific classified failure.
    type Error: ClassifyOperatorFailure;

    /// Counts the same provider-native operation shape later prepared for send.
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

/// Failure annotated with the exact orchestration stage that failed.
#[derive(Debug)]
pub enum ModelCallExecutionError<
    PrepareError,
    FailureError,
    AuthorizationError,
    ProviderError,
    ObservationError,
> {
    /// The prepare-call transaction failed.
    Prepare(PrepareError),
    /// Provider-neutral request rendering failed closed.
    Render(ModelFrontierRenderingError),
    /// Credential lookup or capability preparation failed as an operator error.
    CapabilityPreparation(ProviderError),
    /// The guarded prepared-call failure transaction failed.
    PreparedFailureCommit(FailureError),
    /// Authoritative reread of a retained prepared-call failure failed.
    PreparedFailureReread(FailureError),
    /// Durable send authorization failed.
    Authorization(AuthorizationError),
    /// Authoritative reread after an ambiguous authorization also failed.
    AuthorizationReread {
        /// The original commit-ambiguous authorization failure.
        authorization_error: AuthorizationError,
        /// The failure to establish whether authorization committed.
        reread_error: AuthorizationError,
    },
    /// A later pass still could not reconcile retained non-consumption proof.
    AuthorizationReconciliation(AuthorizationError),
    /// Provider work produced no trustworthy observation.
    Provider(ProviderError),
    /// The terminal-observation transaction failed.
    ObservationCommit {
        /// The failed observation transaction or authoritative reread.
        error: ObservationError,
        /// The unchanged provider observation retained for a later pass.
        retained_observation: CorrelatedModelCallTerminalObservation,
    },
}

impl<PrepareError, FailureError, AuthorizationError, ProviderError, ObservationError> fmt::Display
    for ModelCallExecutionError<
        PrepareError,
        FailureError,
        AuthorizationError,
        ProviderError,
        ObservationError,
    >
where
    PrepareError: fmt::Display,
    FailureError: fmt::Display,
    AuthorizationError: fmt::Display,
    ProviderError: fmt::Display,
    ObservationError: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Prepare(error) => write!(formatter, "model-call prepare stage failed: {error}"),
            Self::Render(error) => write!(formatter, "model-call render stage failed: {error}"),
            Self::CapabilityPreparation(error) => {
                write!(formatter, "model-call capability stage failed: {error}")
            }
            Self::PreparedFailureCommit(error) => {
                write!(
                    formatter,
                    "model-call prepared-failure commit failed: {error}"
                )
            }
            Self::PreparedFailureReread(error) => {
                write!(
                    formatter,
                    "model-call prepared-failure reread failed: {error}"
                )
            }
            Self::Authorization(error) => {
                write!(formatter, "model-call authorization stage failed: {error}")
            }
            Self::AuthorizationReread { reread_error, .. } => {
                write!(
                    formatter,
                    "model-call authorization reread failed: {reread_error}"
                )
            }
            Self::AuthorizationReconciliation(error) => {
                write!(
                    formatter,
                    "model-call authorization reconciliation failed: {error}"
                )
            }
            Self::Provider(error) => write!(formatter, "model-call provider stage failed: {error}"),
            Self::ObservationCommit { error, .. } => {
                write!(formatter, "model-call observation commit failed: {error}")
            }
        }
    }
}

impl<PrepareError, FailureError, AuthorizationError, ProviderError, ObservationError> Error
    for ModelCallExecutionError<
        PrepareError,
        FailureError,
        AuthorizationError,
        ProviderError,
        ObservationError,
    >
where
    PrepareError: Error + 'static,
    FailureError: Error + 'static,
    AuthorizationError: Error + 'static,
    ProviderError: Error + 'static,
    ObservationError: Error + 'static,
{
}

impl<PrepareError, FailureError, AuthorizationError, ProviderError, ObservationError>
    ClassifyOperatorFailure
    for ModelCallExecutionError<
        PrepareError,
        FailureError,
        AuthorizationError,
        ProviderError,
        ObservationError,
    >
where
    PrepareError: ClassifyOperatorFailure,
    FailureError: ClassifyOperatorFailure,
    AuthorizationError: ClassifyOperatorFailure,
    ProviderError: ClassifyOperatorFailure,
    ObservationError: ClassifyOperatorFailure,
{
    fn operator_failure_class(&self) -> OperatorFailureClass {
        match self {
            Self::Prepare(error) => error.operator_failure_class(),
            Self::Render(error) => error.operator_failure_class(),
            Self::CapabilityPreparation(error) | Self::Provider(error) => {
                error.operator_failure_class()
            }
            Self::PreparedFailureCommit(error) | Self::PreparedFailureReread(error) => {
                error.operator_failure_class()
            }
            Self::Authorization(error) => error.operator_failure_class(),
            Self::AuthorizationReread { reread_error, .. } => reread_error.operator_failure_class(),
            Self::AuthorizationReconciliation(error) => error.operator_failure_class(),
            Self::ObservationCommit { error, .. } => error.operator_failure_class(),
        }
    }

    fn operator_failure_cause_code(&self) -> &'static str {
        match self {
            Self::Prepare(_) => "model_call_prepare",
            Self::Render(_) => "model_call_render",
            Self::CapabilityPreparation(_) => "model_call_capability_preparation",
            Self::PreparedFailureCommit(_) => "model_call_prepared_failure_commit",
            Self::PreparedFailureReread(_) => "model_call_prepared_failure_reread",
            Self::Authorization(_) => "model_call_authorization",
            Self::AuthorizationReread { .. } => "model_call_authorization_reread",
            Self::AuthorizationReconciliation(_) => "model_call_authorization_reconciliation",
            Self::Provider(_) => "model_call_provider",
            Self::ObservationCommit { .. } => "model_call_observation_commit",
        }
    }
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
                }) => {
                    break (
                        request,
                        credential_reference,
                        dangerous_tool_auto_approval,
                        recorded_user_overrides,
                        system_prompt,
                        tool_entries,
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
            ModelCallTerminalObservation::CompletedWithTools { response } => {
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
        let ModelCallTerminalObservation::CompletedWithTools { response } = observation else {
            return Box::new([]);
        };
        let mut remaining_overrides: Vec<&RecordedUserOverride> =
            recorded_user_overrides.iter().collect();
        response
            .parts()
            .iter()
            .filter_map(|part| match part {
                AssistantResponsePart::Text(_) => None,
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

/// Sanitized failure from the deterministic scripted provider.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScriptedModelCallError {
    /// No scripted action remained for a requested capability.
    ScriptExhausted,
    /// The script explicitly selected a capability-stage operator failure.
    CapabilityOperatorFailure,
    /// The script explicitly selected an interaction-stage operator failure.
    InteractionOperatorFailure,
    /// Issued authorization did not match the prepared capability.
    AuthorizationMismatch,
}

impl fmt::Display for ScriptedModelCallError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ScriptExhausted => "scripted model-call actions are exhausted",
            Self::CapabilityOperatorFailure => "scripted model-call capability preparation failed",
            Self::InteractionOperatorFailure => "scripted model-call interaction failed",
            Self::AuthorizationMismatch => {
                "scripted model-call authorization does not match its capability"
            }
        })
    }
}

impl Error for ScriptedModelCallError {}

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
mod tests {
    use std::{
        collections::VecDeque,
        io::{self, Write},
        num::NonZeroU64,
        sync::{Arc, Mutex as StdMutex},
    };

    use expect_test::expect;
    use signalbox_domain::{
        AcceptedInputDisposition, AcceptedInputLifecycle, AcceptedInputQueueOrder,
        AcceptedInputSchedulingReconstitutionInput, AcceptedInputStartingLineage,
        AcceptedInputTurnActivationIdentities, AcceptedInputTurnSchedulingRecord,
        AcceptedInputTurnSchedulingRecordState, ActiveTurnSchedulingReconstitutionInput, Actor,
        DecideToolRequest, DeliveryRequest, DirectModelSelection, DurableCommandId,
        FrozenModelSelection, ImportedMessageContentAbsence, ModelCallDisposition,
        ModelCallExecutionReconstitutionInput, ModelCallOriginContent,
        ModelCallReconstitutionInput, ModelCallReconstitutionState, ModelSelectionOverride,
        ModelSelectionRequest, ModelTargetCatalog, ModelTargetDefinition, NormalizedToolArguments,
        PerInputConfigurationChoices, PinnedProviderTargetReconstitutionInput,
        ProviderModelIdentity, ResolvedContextFrontierReconstitutionInput, ResolvedProviderTarget,
        SemanticTranscriptEntryReconstitutionInput, SessionAcceptanceTailEntryReconstitutionInput,
        SessionAcceptanceTailReconstitutionInput, SessionConfigurationDefaults,
        SessionConfigurationDefaultsVersion, SessionCreationCause, SessionCreationProvenance,
        SessionInputPosition, SessionReconstitutionInput, SubmitInput,
        SubmitInputAppliedTurnOriginReconstitutionInput,
        SubmitInputDirectTurnOriginConstructionInput, SubmitInputReconstitutionInput,
        SubmitInputTurnOriginReconstitutionInput, ToolApprovalResolutionReconstitutionInput,
        ToolAttemptReconstitutionInput, ToolAttemptReconstitutionState, ToolDispatchGeneration,
        ToolEffectClass, ToolName, ToolPermissionDefault, ToolRequestOrdinal,
        ToolRequestReconstitutionInput, ToolResultText, TranscriptAncestry,
    };
    use tracing::instrument::WithSubscriber as _;
    use uuid::Uuid;

    use super::*;

    #[derive(Clone, Default)]
    struct CapturedTelemetry(Arc<StdMutex<Vec<u8>>>);

    struct CapturedTelemetryWriter(Arc<StdMutex<Vec<u8>>>);

    impl Write for CapturedTelemetryWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .expect("captured telemetry remains available")
                .extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'writer> tracing_subscriber::fmt::MakeWriter<'writer> for CapturedTelemetry {
        type Writer = CapturedTelemetryWriter;

        fn make_writer(&'writer self) -> Self::Writer {
            CapturedTelemetryWriter(Arc::clone(&self.0))
        }
    }

    impl CapturedTelemetry {
        fn text(&self) -> String {
            String::from_utf8(
                self.0
                    .lock()
                    .expect("captured telemetry remains available")
                    .clone(),
            )
            .expect("captured telemetry is UTF-8")
        }
    }

    fn identity<Identity>(value: u128, from_uuid: impl FnOnce(Uuid) -> Identity) -> Identity {
        from_uuid(Uuid::from_u128(value))
    }

    fn credential_reference() -> ModelCallCredentialReference {
        ModelCallCredentialReference::new("fixture-provider-primary")
    }

    fn rendered_text(content: UserContent) -> ModelUserContent {
        render_model_user_content(content, |_| None)
            .expect("text-only fixture needs no attachment catalog facts")
    }

    /// S02: ordered attachment content becomes bounded
    /// canonical text stubs with metadata visible and blob bytes absent.
    #[test]
    fn s02_attachment_frontier_renders_ordered_stubs_without_bytes() {
        let digest = BlobDigest::digest(b"secret blob bytes");
        let filename =
            signalbox_domain::AttachmentDisplayFilename::try_new(String::from("scan\".png"))
                .expect("the fixture display filename is valid");
        let content = UserContent::try_parts(vec![
            UserContentPart::try_text(String::from("before"))
                .expect("the fixture leading text is valid"),
            UserContentPart::Attachment {
                digest,
                kind: AttachmentKind::Image,
                media_type: signalbox_domain::DeclaredMediaType::try_new(String::from("image/png"))
                    .expect("the fixture media type is valid"),
                display_filename: Some(filename),
            },
            UserContentPart::try_text(String::from("after"))
                .expect("the fixture trailing text is valid"),
        ])
        .expect("the interleaved fixture content is valid");
        let expected_stub = format!(
            r#"{{"signalbox_attachment":{{"kind":"image","media_type":"image/png","display_filename":"scan\".png","byte_length":"17","digest":"{digest}"}}}}"#
        );

        let rendered = render_model_user_content(content, |candidate| {
            (candidate == digest)
                .then_some(NonZeroU64::new(17).expect("the fixture attachment length is positive"))
        })
        .expect("the catalog fact covers the exact attachment");

        assert_eq!(rendered.parts()[0].as_str(), "before");
        assert_eq!(rendered.parts()[1].as_str(), expected_stub);
        assert_eq!(rendered.parts()[2].as_str(), "after");
        assert!(!rendered.parts()[1].as_str().contains("secret blob bytes"));
    }

    #[test]
    fn maximum_checked_attachment_metadata_fits_the_named_stub_bound() {
        let digest = BlobDigest::digest(b"maximum metadata fixture");
        let filename = signalbox_domain::AttachmentDisplayFilename::try_new("\u{1}".repeat(255))
            .expect("the maximum-byte control filename is valid metadata");
        let media_type = signalbox_domain::DeclaredMediaType::try_new("\"".repeat(255))
            .expect("the maximum-byte visible-ASCII media type is valid");
        let content = UserContent::try_parts(vec![UserContentPart::Attachment {
            digest,
            kind: AttachmentKind::Document,
            media_type,
            display_filename: Some(filename),
        }])
        .expect("the maximum metadata fixture is valid");

        let rendered = render_model_user_content(content, |_| Some(NonZeroU64::MAX))
            .expect("the derived bound covers maximum checked metadata");
        let stub = rendered.parts()[0].as_str();

        assert!(stub.len() <= MAX_RENDERED_ATTACHMENT_STUB_BYTES);
        assert_eq!(stub.len(), 2_242);
    }

    #[test]
    fn delegation_task_message_and_background_result_render_as_typed_inputs() {
        let child = identity(40, SessionId::from_uuid);
        let parent = identity(41, SessionId::from_uuid);
        let spawning_request = identity(42, ToolRequestId::from_uuid);
        let awaiting_request = identity(43, ToolRequestId::from_uuid);
        let message = identity(44, DelegationMessageId::from_uuid);
        let parent_turn = identity(45, TurnId::from_uuid);
        let child_turn = identity(46, TurnId::from_uuid);
        let task_source = SemanticTranscriptEntryRef::from_source(
            child,
            identity(47, SemanticTranscriptEntryId::from_uuid),
        );
        let message_source = SemanticTranscriptEntryRef::from_source(
            child,
            identity(48, SemanticTranscriptEntryId::from_uuid),
        );
        let result_source = SemanticTranscriptEntryRef::from_source(
            parent,
            identity(49, SemanticTranscriptEntryId::from_uuid),
        );
        let task_content =
            DelegationContent::try_new("delegated work".into()).expect("fixture task is valid");
        let message_content =
            DelegationContent::try_new("peer update".into()).expect("fixture message is valid");
        let result_content =
            DelegationContent::try_new("delivered result".into()).expect("fixture result is valid");
        let outcome = DelegationOutcome::reconstitute(
            signalbox_domain::DelegationOutcomeKind::ResultReturned,
            Some(result_content),
            signalbox_domain::DelegationOutcomeReason::ChildCompleted,
            signalbox_domain::DelegationProvenanceReconstitutionInput::ChildTurn {
                session: child,
                turn: child_turn,
            },
        )
        .expect("fixture child outcome is correlated");
        let task = SemanticTranscriptEntryPayload::DelegatedTask {
            spawning_request,
            parent_session: parent,
            parent_turn,
            content: task_content.clone(),
        };
        let peer_message = SemanticTranscriptEntryPayload::DelegationMessage {
            spawning_request,
            message,
            sender: parent,
            recipient: child,
            delivery_sequence: NonZeroU64::MIN,
            content: message_content.clone(),
        };
        let result = SemanticTranscriptEntryPayload::DelegationResult {
            awaiting_request,
            spawning_request,
            child,
            mode: DelegationWaitMode::Background,
            delivery_sequence: Some(NonZeroU64::new(2).expect("two is positive")),
            outcome: Box::new(outcome.clone()),
        };

        let rendered = render_frontier_messages(
            [
                (task_source, &task),
                (message_source, &peer_message),
                (result_source, &result),
            ],
            |_| None,
            |_| None,
            [],
        )
        .expect("typed delegation entries render without accepted-input evidence");

        assert_eq!(
            rendered.as_ref(),
            &[
                ModelConversationMessage::DelegatedTask {
                    source: task_source,
                    spawning_request,
                    parent_session: parent,
                    parent_turn,
                    content: task_content,
                },
                ModelConversationMessage::DelegationMessage {
                    source: message_source,
                    spawning_request,
                    message,
                    sender: parent,
                    recipient: child,
                    delivery_sequence: NonZeroU64::MIN,
                    content: message_content,
                },
                ModelConversationMessage::BackgroundDelegationResult {
                    source: result_source,
                    awaiting_request,
                    spawning_request,
                    child,
                    delivery_sequence: NonZeroU64::new(2).expect("two is positive"),
                    outcome,
                },
            ]
        );
    }

    #[test]
    fn foreground_delegation_result_renders_as_await_tool_result() {
        let parent = identity(50, SessionId::from_uuid);
        let child = identity(51, SessionId::from_uuid);
        let awaiting_request = identity(52, ToolRequestId::from_uuid);
        let spawning_request = identity(53, ToolRequestId::from_uuid);
        let source = SemanticTranscriptEntryRef::from_source(
            parent,
            identity(54, SemanticTranscriptEntryId::from_uuid),
        );
        let outcome = DelegationOutcome::reconstitute(
            signalbox_domain::DelegationOutcomeKind::ChildFailed,
            None,
            signalbox_domain::DelegationOutcomeReason::ChildExecutionFailed,
            signalbox_domain::DelegationProvenanceReconstitutionInput::ChildTurn {
                session: child,
                turn: identity(55, TurnId::from_uuid),
            },
        )
        .expect("fixture failure is correlated");
        let result = SemanticTranscriptEntryPayload::DelegationResult {
            awaiting_request,
            spawning_request,
            child,
            mode: DelegationWaitMode::Foreground,
            delivery_sequence: None,
            outcome: Box::new(outcome.clone()),
        };

        let rendered = render_frontier_messages([(source, &result)], |_| None, |_| None, [])
            .expect("foreground delivery is one correlated tool result");

        assert_eq!(
            rendered.as_ref(),
            &[ModelConversationMessage::ToolResult {
                source,
                request: awaiting_request,
                content: ModelToolResultContent::Delegation(outcome),
            }]
        );
    }

    fn ready(request: PreparedModelCallRequest) -> PrepareModelCallOutcome {
        PrepareModelCallOutcome::Ready {
            request: Box::new(request),
            credential_reference: credential_reference(),
            dangerous_tool_auto_approval: DangerousToolAutoApproval::Disabled,
            recorded_user_overrides: Box::new([]),
            system_prompt: None,
            tool_entries: Box::new([]),
        }
    }

    /// The same reload outcome as [`ready`], carrying the exact durable
    /// authority for every tool-related entry the request's frontier names.
    fn ready_with_tool_evidence(
        request: PreparedModelCallRequest,
        tool_entries: Box<[ResolvedToolConversationEntry]>,
    ) -> PrepareModelCallOutcome {
        PrepareModelCallOutcome::Ready {
            request: Box::new(request),
            credential_reference: credential_reference(),
            dangerous_tool_auto_approval: DangerousToolAutoApproval::Disabled,
            recorded_user_overrides: Box::new([]),
            system_prompt: None,
            tool_entries,
        }
    }

    fn tool_response() -> ModelCallTerminalObservation {
        let arguments =
            signalbox_domain::NormalizedToolArguments::try_from_provider_text(String::from("{}"))
                .expect("fixture arguments are valid");
        let parts = vec![
            AssistantResponsePart::Text(
                AssistantText::try_new(String::from("checking"))
                    .expect("fixture assistant text is valid"),
            ),
            AssistantResponsePart::ToolCall(signalbox_domain::ToolCallProposal::new(
                signalbox_domain::ToolName::try_new(String::from("automatic"))
                    .expect("fixture tool name is valid"),
                arguments.clone(),
            )),
            AssistantResponsePart::ToolCall(signalbox_domain::ToolCallProposal::new(
                signalbox_domain::ToolName::try_new(String::from("unknown"))
                    .expect("fixture tool name is valid"),
                arguments,
            )),
        ];
        ModelCallTerminalObservation::CompletedWithTools {
            response: signalbox_domain::ToolUsingAssistantResponse::try_from_parts(parts)
                .expect("fixture response contains tools"),
        }
    }

    /// One request in the canonical model-rendering session, turn, and call.
    ///
    /// The request identity derives from the ordinal and is deliberately in a
    /// different UUID range so an implementation cannot confuse the two.
    fn model_tool_request(ordinal: u32) -> ToolRequest {
        ToolRequestReconstitutionInput::new(
            identity(100 + u128::from(ordinal), ToolRequestId::from_uuid),
            identity(1, SessionId::from_uuid),
            identity(2, TurnId::from_uuid),
            identity(3, ModelCallId::from_uuid),
            ToolRequestOrdinal::from_u32(ordinal),
            ToolName::try_new(format!("tool_{ordinal}")).expect("fixture tool name is valid"),
            NormalizedToolArguments::try_from_provider_text(String::from("{}"))
                .expect("fixture arguments are valid"),
        )
        .into_request()
    }

    fn model_tool_use_message(
        request_identity: u128,
        turn_identity: u128,
        call_identity: u128,
        ordinal: u32,
    ) -> ModelConversationMessage {
        let session = identity(1, SessionId::from_uuid);
        let turn = identity(turn_identity, TurnId::from_uuid);
        let producing_call = identity(call_identity, ModelCallId::from_uuid);
        let request = ToolRequestReconstitutionInput::new(
            identity(request_identity, ToolRequestId::from_uuid),
            session,
            turn,
            producing_call,
            ToolRequestOrdinal::from_u32(ordinal),
            ToolName::try_new(String::from("known")).expect("fixture tool name is valid"),
            NormalizedToolArguments::try_from_provider_text(String::from("{}"))
                .expect("fixture arguments are valid"),
        )
        .into_request();
        ModelConversationMessage::AssistantToolUse {
            source: SemanticTranscriptEntryRef::from_source(
                session,
                identity(
                    request_identity + 10_000,
                    SemanticTranscriptEntryId::from_uuid,
                ),
            ),
            producing_call,
            request,
        }
    }

    fn prepared_fixture() -> (PreparedModelCallRequest, AuthorizedModelCall) {
        let prepared_execution = prepared_execution_fixture();
        let request = prepared_execution
            .resume_prepared_call()
            .expect("fixture Prepared request resumes");
        let authorized = prepared_execution
            .authorize_send()
            .expect("fixture Prepared call authorizes");
        (request, authorized)
    }

    /// The terminal failed turn a capability-failure commit records for the
    /// fixture's prepared call.
    fn failed_turn_fixture() -> FailedModelCallTurn {
        prepared_execution_fixture()
            .fail_prepared_call(FailedModelCallTurnIdentities::new(
                identity(120, SemanticTranscriptEntryId::from_uuid),
                identity(121, ContextFrontierId::from_uuid),
            ))
            .expect("a prepared fixture call closes as a failed turn")
    }

    fn prepared_execution_fixture() -> signalbox_domain::ModelCallExecution {
        let session_id = identity(1, SessionId::from_uuid);
        let direct = identity(2, DirectModelSelection::from_uuid);
        let accepted_input = identity(3, AcceptedInputId::from_uuid);
        let turn_id = identity(4, TurnId::from_uuid);
        let command_id = identity(5, DurableCommandId::from_uuid);
        let version = SessionConfigurationDefaultsVersion::first();
        let defaults = SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(direct));
        let session = SessionReconstitutionInput::new(
            session_id,
            session_id,
            SessionCreationProvenance::new(
                SessionCreationCause::Interactive,
                TranscriptAncestry::None,
            ),
            session_id,
            version,
            session_id,
            version,
            defaults.clone(),
            signalbox_domain::SessionPlacementReconstitutionFacts {
                current_pointer_session: session_id,
                current_pointer_version: signalbox_domain::SessionPlacementVersion::INITIAL,
                selected_event_session: session_id,
                selected_event: signalbox_domain::VersionedSessionPlacement::initial(
                    signalbox_domain::SessionPlacement::pathless(),
                ),
            },
        )
        .reconstitute()
        .expect("fixture Session facts are correlated");
        let choices =
            PerInputConfigurationChoices::new(version, ModelSelectionOverride::UseSessionDefault);
        let delivery = DeliveryRequest::StartWhenNoActiveTurn {
            configuration: choices,
        };
        let content = UserContent::try_text(String::from("exact user request"))
            .expect("fixture content is valid");
        let command = SubmitInput::new(command_id, session_id, content.clone(), delivery);
        let position = SessionInputPosition::first();
        let order = AcceptedInputQueueOrder::ordinary(position);
        let lifecycle = AcceptedInputLifecycle::new(
            accepted_input,
            AcceptedInputDisposition::OriginOf(turn_id),
        );
        let receipt = SubmitInputReconstitutionInput::applied_turn_origin(
            SubmitInputAppliedTurnOriginReconstitutionInput {
                command,
                stored_actor: Actor::User,
                result_session: session_id,
                result_accepted_input: accepted_input,
                result_turn: turn_id,
                predecessor_origin: None,
                non_accepted_predecessor: None,
                accepted_command: command_id,
                accepted_input,
                accepted_session: session_id,
                accepted_content: content,
                accepted_delivery: delivery,
                accepted_position: position,
                accepted_disposition: AcceptedInputDisposition::OriginOf(turn_id),
                queue_session: session_id,
                queue_turn: turn_id,
                queue_order: order,
                defaults_session: session_id,
                defaults_version: version,
                defaults,
                stored_requested_model: ModelSelectionRequest::Direct(direct),
                stored_frozen_model: FrozenModelSelection::Direct(direct),
                stored_model_settings: None,
                stored_model_settings_adjustments: Vec::new(),
            },
        )
        .reconstitute()
        .expect("fixture receipt facts are correlated");
        let origin = SubmitInputTurnOriginReconstitutionInput::new(
            SubmitInputDirectTurnOriginConstructionInput {
                receipt,
                lifecycle: lifecycle.clone(),
                queue_accepted_input: accepted_input,
                queue_session: session_id,
                queue_turn: turn_id,
                queue_order: order,
            },
        );
        let origin_content = ModelCallOriginContent::from_reconstituted_turn_origin(&origin)
            .expect("checked origin carries exact content");
        let checked = session
            .current_configuration_defaults()
            .derive_request(version, ModelSelectionOverride::UseSessionDefault)
            .expect("fixture defaults version is current");
        let configuration = signalbox_domain::OriginConfiguration::freeze(checked, |_| None)
            .expect("a direct selection needs no alias lookup");
        let record = AcceptedInputTurnSchedulingRecord::new(
            session_id,
            turn_id,
            session_id,
            lifecycle,
            session_id,
            turn_id,
            order,
            delivery,
            configuration,
            AcceptedInputTurnSchedulingRecordState::Queued,
        );
        let activation = AcceptedInputSchedulingReconstitutionInput::new(
            session,
            vec![record],
            Vec::new(),
            Vec::new(),
            None,
        )
        .reconstitute()
        .expect("fixture scheduling projection is complete")
        .prepare_earliest_queued_activation(AcceptedInputTurnActivationIdentities::new(
            identity(99, SemanticTranscriptEntryId::from_uuid),
            identity(6, SemanticTranscriptEntryId::from_uuid),
            identity(7, ContextFrontierId::from_uuid),
            identity(8, TurnAttemptId::from_uuid),
        ))
        .expect("the sole queued fixture turn is eligible");
        let (active_turn, starting_entries, starting_snapshot) = activation.into_parts();
        let origin_entry = starting_entries
            .last()
            .expect("fixture activation carries its origin")
            .clone();
        let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
            direct,
            ResolvedProviderTarget::naming(identity(9, ProviderModelIdentity::from_uuid)),
        )])
        .expect("the fixture target key is unique");
        let initial = ModelCallExecutionReconstitutionInput::new(
            active_turn.clone(),
            targets.clone(),
            starting_snapshot.clone(),
            vec![origin_entry.clone()],
            vec![origin_content.clone()],
            None,
            Vec::new(),
        )
        .reconstitute()
        .expect("fixture activation reconstructs execution");
        let prepared = initial
            .prepare_initial_call(identity(10, ModelCallId::from_uuid))
            .expect("fixture call can be prepared");
        ModelCallExecutionReconstitutionInput::new(
            active_turn,
            targets,
            starting_snapshot,
            vec![origin_entry],
            vec![origin_content],
            Some(PinnedProviderTargetReconstitutionInput::new(
                prepared.call().turn(),
                prepared.call().target(),
            )),
            vec![ModelCallReconstitutionInput::new(
                prepared.call().id(),
                prepared.call().turn(),
                prepared.call().attempt(),
                prepared.call().selection(),
                prepared.call().target(),
                prepared.call().frontier().snapshot(),
                ModelCallReconstitutionState::Prepared,
            )],
        )
        .reconstitute()
        .expect("fixture Prepared facts reconstruct")
    }

    /// A `Prepared` continuation call whose turn already recorded exactly
    /// `rounds` distinct automatic tool rounds, paired with the exact durable
    /// tool authority its frontier rendering demands.
    ///
    /// Each round is one completed producing call contributing a single
    /// `AssistantToolUse` frontier entry paired with its own `ToolDenied`
    /// result, so the recorded round count is the one knob. Every round is
    /// denied, which closes it and leaves the continuation call admissible;
    /// pairing each proposal with its result is what the tool loop actually
    /// produces, and a proposal-only history is a shape no round can reach.
    /// Identities are seeded from a dedicated range, decorrelated from the
    /// round ordinal, so an implementation counting identities instead of
    /// producing calls cannot accidentally pass.
    ///
    /// The returned failed turn closes *this* fixture's own prepared call, so
    /// a test can require the committed terminalization to name the saturated
    /// session and call rather than accepting any failed turn at all.
    fn tool_round_saturated_fixture(
        rounds: usize,
    ) -> (
        PreparedModelCallRequest,
        Box<[ResolvedToolConversationEntry]>,
        FailedModelCallTurn,
    ) {
        tool_round_saturated_fixture_with_assistant_text(rounds, None)
    }

    /// The same saturated turn, optionally carrying one assistant-text entry
    /// alongside the last round's proposal.
    ///
    /// Assistant text is durable frontier content the renderer clones but no
    /// tool evidence names, which is what lets a test separate the retained
    /// content a tool-only accounting sees from the content a render actually
    /// holds.
    fn tool_round_saturated_fixture_with_assistant_text(
        rounds: usize,
        assistant_text: Option<&str>,
    ) -> (
        PreparedModelCallRequest,
        Box<[ResolvedToolConversationEntry]>,
        FailedModelCallTurn,
    ) {
        let session_id = identity(200, SessionId::from_uuid);
        let direct = identity(201, DirectModelSelection::from_uuid);
        let accepted_input = identity(202, AcceptedInputId::from_uuid);
        let turn_id = identity(203, TurnId::from_uuid);
        let origin_entry = SemanticTranscriptEntryRef::from_source(
            session_id,
            identity(204, SemanticTranscriptEntryId::from_uuid),
        );
        let starting_frontier = identity(205, ContextFrontierId::from_uuid);
        let current_attempt = identity(206, TurnAttemptId::from_uuid);
        let target =
            ResolvedProviderTarget::naming(identity(207, ProviderModelIdentity::from_uuid));
        let continuation_call = identity(208, ModelCallId::from_uuid);
        let current_frontier = identity(209, ContextFrontierId::from_uuid);
        let version = SessionConfigurationDefaultsVersion::first();
        let defaults = SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(direct));
        let session = SessionReconstitutionInput::new(
            session_id,
            session_id,
            SessionCreationProvenance::new(
                SessionCreationCause::Interactive,
                TranscriptAncestry::None,
            ),
            session_id,
            version,
            session_id,
            version,
            defaults.clone(),
            signalbox_domain::SessionPlacementReconstitutionFacts {
                current_pointer_session: session_id,
                current_pointer_version: signalbox_domain::SessionPlacementVersion::INITIAL,
                selected_event_session: session_id,
                selected_event: signalbox_domain::VersionedSessionPlacement::initial(
                    signalbox_domain::SessionPlacement::pathless(),
                ),
            },
        )
        .reconstitute()
        .expect("fixture Session facts are correlated");
        let delivery = DeliveryRequest::StartWhenNoActiveTurn {
            configuration: PerInputConfigurationChoices::new(
                version,
                ModelSelectionOverride::UseSessionDefault,
            ),
        };
        let content = UserContent::try_text(String::from("keep using tools"))
            .expect("fixture content is valid");
        let position = SessionInputPosition::first();
        let lifecycle = AcceptedInputLifecycle::new(
            accepted_input,
            AcceptedInputDisposition::OriginOf(turn_id),
        );
        let checked = session
            .current_configuration_defaults()
            .derive_request(version, ModelSelectionOverride::UseSessionDefault)
            .expect("fixture defaults version is current");
        let configuration = signalbox_domain::OriginConfiguration::freeze(checked, |_| None)
            .expect("a direct selection needs no alias lookup");
        let selection = *configuration.effective().model();
        let requests = (0_u128..)
            .take(rounds)
            .map(|round| {
                ToolRequestReconstitutionInput::new(
                    identity(2_000 + round, ToolRequestId::from_uuid),
                    session_id,
                    turn_id,
                    identity(1_000 + round, ModelCallId::from_uuid),
                    ToolRequestOrdinal::from_u32(0),
                    ToolName::try_new(String::from("saturating"))
                        .expect("fixture tool name is valid"),
                    NormalizedToolArguments::try_from_provider_text(String::from("{}"))
                        .expect("fixture arguments are valid"),
                )
                .into_request()
            })
            .collect::<Vec<_>>();
        let tool_use_entries = (0_u128..)
            .zip(&requests)
            .map(|(round, _)| {
                SemanticTranscriptEntryRef::from_source(
                    session_id,
                    identity(3_000 + round, SemanticTranscriptEntryId::from_uuid),
                )
            })
            .collect::<Vec<_>>();
        // Every round carries its own result. A continuation frontier must
        // include the current round's complete result evidence, and proposals
        // render paired with their results, so a history of `rounds` proposals
        // closed by a single result is a shape the tool loop cannot produce.
        // Denying each round is the cheapest spec-conformant pairing: it closes
        // the round it belongs to and leaves the continuation admissible.
        let denial_entries = (0_u128..)
            .zip(&requests)
            .map(|(round, _)| {
                SemanticTranscriptEntryRef::from_source(
                    session_id,
                    identity(5_000 + round, SemanticTranscriptEntryId::from_uuid),
                )
            })
            .collect::<Vec<_>>();
        let denials = (0_u128..)
            .zip(&requests)
            .map(|(round, request)| {
                ToolApprovalResolutionReconstitutionInput::user_command(
                    DecideToolRequest::try_new(
                        identity(6_000 + round, DurableCommandId::from_uuid),
                        request.id(),
                        ToolApprovalDecision::Deny {
                            reason: Some(
                                ToolDenialReason::try_new(String::from("fixture closes the round"))
                                    .expect("fixture denial reason is valid"),
                            ),
                        },
                    )
                    .expect("the fixture command identity is admitted")
                    .prepare_applied(request)
                    .expect("the command names the exact request"),
                )
                .reconstitute()
                .expect("user denial provenance is implemented")
            })
            .collect::<Vec<_>>();
        // The text is produced by the last round's own call and precedes that
        // round's proposal, which is how a provider response carrying both text
        // and a tool request lands. Appending it after the round's results
        // instead would leave the latest round unclosed, which is a frontier
        // shape the tool loop cannot reach.
        let assistant_entry = assistant_text.map(|text| {
            (
                SemanticTranscriptEntryRef::from_source(
                    session_id,
                    identity(8_000, SemanticTranscriptEntryId::from_uuid),
                ),
                requests
                    .last()
                    .expect("the fixture carries at least one round")
                    .producing_call(),
                AssistantText::try_new(String::from(text))
                    .expect("fixture assistant text is valid"),
            )
        });
        let semantic_entries = [SemanticTranscriptEntryReconstitutionInput::new(
            origin_entry.entry(),
            session_id,
            SemanticTranscriptEntryPayload::OriginAcceptedInput { accepted_input },
        )]
        .into_iter()
        .chain(
            tool_use_entries
                .iter()
                .zip(&denial_entries)
                .zip(&requests)
                .enumerate()
                .flat_map(|(round, ((proposal, result), request))| {
                    let text = assistant_entry
                        .iter()
                        .filter(|_| round + 1 == rounds)
                        .map(|(source, producing_call, value)| {
                            SemanticTranscriptEntryReconstitutionInput::new(
                                source.entry(),
                                session_id,
                                SemanticTranscriptEntryPayload::AssistantText {
                                    producing_call: *producing_call,
                                    value: value.clone(),
                                },
                            )
                        })
                        .collect::<Vec<_>>();
                    text.into_iter().chain([
                        SemanticTranscriptEntryReconstitutionInput::new(
                            proposal.entry(),
                            session_id,
                            SemanticTranscriptEntryPayload::AssistantToolUse {
                                producing_call: request.producing_call(),
                                request: request.id(),
                            },
                        ),
                        SemanticTranscriptEntryReconstitutionInput::new(
                            result.entry(),
                            session_id,
                            SemanticTranscriptEntryPayload::ToolDenied {
                                request: request.id(),
                            },
                        ),
                    ])
                }),
        )
        .collect::<Vec<_>>();
        // Only the first round is prepared from the turn's starting frontier.
        // Every continuation call is prepared from the preceding round's
        // *result* frontier, which already contains that round's proposal and
        // its paired result — so round `n` sees the origin plus `n` pairs.
        // Giving all `rounds` calls the starting snapshot would describe a
        // history the implemented tool loop cannot produce, letting the
        // saturation bound be exercised against an impossible turn.
        let round_frontiers = (0_u128..)
            .take(rounds)
            .map(|round| {
                if round == 0 {
                    starting_frontier
                } else {
                    identity(7_000 + round, ContextFrontierId::from_uuid)
                }
            })
            .collect::<Vec<_>>();
        let round_snapshots = round_frontiers
            .iter()
            .enumerate()
            .map(|(preceding_rounds, frontier)| {
                ResolvedContextFrontierReconstitutionInput::new(
                    session_id,
                    *frontier,
                    [origin_entry]
                        .into_iter()
                        .chain(
                            tool_use_entries
                                .iter()
                                .zip(&denial_entries)
                                .take(preceding_rounds)
                                .flat_map(|(proposal, result)| [*proposal, *result]),
                        )
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<Vec<_>>();
        let producing_calls = (0_u128..)
            .zip(&requests)
            .zip(&round_frontiers)
            .map(|((round, request), frontier)| {
                ModelCallReconstitutionInput::new(
                    request.producing_call(),
                    turn_id,
                    identity(4_000 + round, TurnAttemptId::from_uuid),
                    selection,
                    target,
                    *frontier,
                    ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
                )
            })
            .collect::<Vec<_>>();
        let projection = AcceptedInputSchedulingReconstitutionInput::new(
            session,
            vec![AcceptedInputTurnSchedulingRecord::new(
                session_id,
                turn_id,
                session_id,
                lifecycle.clone(),
                session_id,
                turn_id,
                AcceptedInputQueueOrder::ordinary(position),
                delivery,
                configuration,
                AcceptedInputTurnSchedulingRecordState::Active {
                    starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                    starting_frontier,
                    phase: ActiveTurnSchedulingReconstitutionInput::prepared(
                        turn_id,
                        current_attempt,
                    ),
                },
            )],
            semantic_entries,
            round_snapshots,
            Some(SessionAcceptanceTailReconstitutionInput::new(
                session_id,
                accepted_input,
                position,
                vec![SessionAcceptanceTailEntryReconstitutionInput::new(
                    session_id, lifecycle, position, delivery,
                )],
            )),
        )
        .with_model_call_facts(
            vec![PinnedProviderTargetReconstitutionInput::new(
                turn_id, target,
            )],
            producing_calls,
        )
        .reconstitute()
        .expect("the saturated scheduling facts are complete");
        let active_turn = projection
            .active_turn_execution()
            .expect("the saturated turn owns the active slot");
        let starting_snapshot = projection
            .resolved_snapshot(starting_frontier)
            .cloned()
            .expect("the starting snapshot is projected");
        // Proposal-ordered: each round's proposal is immediately followed by
        // its own result, which is how the renderer pairs them.
        let frontier_references = [origin_entry]
            .into_iter()
            .chain(
                tool_use_entries
                    .iter()
                    .zip(&denial_entries)
                    .enumerate()
                    .flat_map(|(round, (proposal, result))| {
                        let text = assistant_entry
                            .iter()
                            .filter(|_| round + 1 == rounds)
                            .map(|(source, _, _)| *source)
                            .collect::<Vec<_>>();
                        text.into_iter().chain([*proposal, *result])
                    }),
            )
            .collect::<Vec<_>>();
        let frontier_entries = frontier_references
            .iter()
            .map(|reference| {
                projection
                    .semantic_entry(*reference)
                    .cloned()
                    .expect("every frontier member is projected")
            })
            .collect::<Vec<_>>();
        let execution = ModelCallExecutionReconstitutionInput::new(
            active_turn,
            ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(direct, target)])
                .expect("the fixture target key is unique"),
            starting_snapshot,
            frontier_entries,
            vec![ModelCallOriginContent::from_goal_turn(
                accepted_input,
                content,
            )],
            Some(PinnedProviderTargetReconstitutionInput::new(
                turn_id, target,
            )),
            vec![ModelCallReconstitutionInput::new(
                continuation_call,
                turn_id,
                current_attempt,
                selection,
                target,
                current_frontier,
                ModelCallReconstitutionState::Prepared,
            )],
        )
        .with_tool_denial_correlations(denials.clone())
        .with_call_snapshot(ResolvedContextFrontierReconstitutionInput::new(
            session_id,
            current_frontier,
            frontier_references,
        ))
        .reconstitute()
        .expect("the saturated Prepared facts reconstruct");
        let tool_evidence = tool_use_entries
            .iter()
            .zip(&denial_entries)
            .zip(&requests)
            .zip(&denials)
            .flat_map(|(((proposal, result), request), approval)| {
                [
                    ResolvedToolConversationEntry::AssistantToolUse {
                        source: *proposal,
                        request: request.clone(),
                    },
                    ResolvedToolConversationEntry::Denied {
                        source: *result,
                        request: request.clone(),
                        approval: approval.clone(),
                    },
                ]
            })
            .collect::<Box<[_]>>();
        let request = execution
            .resume_prepared_call()
            .expect("the saturated Prepared request resumes");
        let failed = execution
            .fail_prepared_call(FailedModelCallTurnIdentities::new(
                identity(212, SemanticTranscriptEntryId::from_uuid),
                identity(213, ContextFrontierId::from_uuid),
            ))
            .expect("the saturated Prepared call closes as a failed turn");
        (request, tool_evidence, failed)
    }

    #[derive(Debug)]
    struct FixedIds {
        calls: VecDeque<ModelCallId>,
        entries: VecDeque<SemanticTranscriptEntryId>,
        frontiers: VecDeque<ContextFrontierId>,
        requests: VecDeque<ToolRequestId>,
        attempts: VecDeque<TurnAttemptId>,
        turns: VecDeque<TurnId>,
    }

    impl FixedIds {
        fn baseline() -> Self {
            Self {
                calls: [20, 21]
                    .map(|value| identity(value, ModelCallId::from_uuid))
                    .into(),
                entries: (30..40)
                    .map(|value| identity(value, SemanticTranscriptEntryId::from_uuid))
                    .collect(),
                frontiers: (40..50)
                    .map(|value| identity(value, ContextFrontierId::from_uuid))
                    .collect(),
                requests: (60..70)
                    .map(|value| identity(value, ToolRequestId::from_uuid))
                    .collect(),
                attempts: (70..80)
                    .map(|value| identity(value, TurnAttemptId::from_uuid))
                    .collect(),
                turns: (50..60)
                    .map(|value| identity(value, TurnId::from_uuid))
                    .collect(),
            }
        }
    }

    impl ModelCallExecutionIdGenerator for FixedIds {
        fn next_model_call_id(&mut self) -> ModelCallId {
            self.calls.pop_front().expect("fixture call identity")
        }

        fn next_semantic_entry_id(&mut self) -> SemanticTranscriptEntryId {
            self.entries.pop_front().expect("fixture entry identity")
        }

        fn next_context_frontier_id(&mut self) -> ContextFrontierId {
            self.frontiers
                .pop_front()
                .expect("fixture frontier identity")
        }

        fn next_tool_request_id(&mut self) -> ToolRequestId {
            self.requests.pop_front().expect("fixture request identity")
        }

        fn next_turn_attempt_id(&mut self) -> TurnAttemptId {
            self.attempts.pop_front().expect("fixture attempt identity")
        }

        fn next_turn_id(&mut self) -> TurnId {
            self.turns.pop_front().expect("fixture turn identity")
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum FakeError {
        IdentityCollision,
        Infrastructure,
        CommitAmbiguous,
    }

    impl fmt::Display for FakeError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(match self {
                Self::IdentityCollision => "fake identity collision",
                Self::Infrastructure => "fake infrastructure failure",
                Self::CommitAmbiguous => "fake commit-ambiguous failure",
            })
        }
    }

    impl Error for FakeError {}

    impl ClassifyOperatorFailure for FakeError {
        fn operator_failure_class(&self) -> OperatorFailureClass {
            match self {
                Self::IdentityCollision => OperatorFailureClass::IdentityCollision,
                Self::Infrastructure => OperatorFailureClass::Infrastructure {
                    commit_ambiguous: false,
                },
                Self::CommitAmbiguous => OperatorFailureClass::Infrastructure {
                    commit_ambiguous: true,
                },
            }
        }
    }

    #[derive(Debug)]
    struct FakePrepare {
        outcomes: VecDeque<Result<PrepareModelCallOutcome, FakeError>>,
        calls: usize,
    }

    impl PrepareModelCallTransaction for FakePrepare {
        type Error = FakeError;

        async fn prepare<NextSteeringIdentities>(
            &mut self,
            _session: SessionId,
            _call: ModelCallId,
            _failure_identities: FailedModelCallTurnIdentities,
            _steering_frontier: ContextFrontierId,
            _next_steering_identities: NextSteeringIdentities,
        ) -> Result<PrepareModelCallOutcome, Self::Error>
        where
            NextSteeringIdentities:
                FnMut(AcceptedInputId) -> (SemanticTranscriptEntryId, TurnId) + Send,
        {
            self.calls += 1;
            self.outcomes
                .pop_front()
                .expect("one fake prepare outcome per call")
        }
    }

    #[derive(Debug)]
    struct UnusedFailure;

    impl FailPreparedModelCallTransaction for UnusedFailure {
        type Error = FakeError;

        async fn fail_prepared<NextTurn>(
            &mut self,
            _session: SessionId,
            _call: ModelCallId,
            _cause: PreparedModelCallFailureCause,
            _attachment_failure: Option<AttachmentPreparationFailure>,
            _identities: FailedModelCallTurnIdentities,
            _next_reclassified_turn: NextTurn,
        ) -> Result<FailedModelCallTurn, Self::Error>
        where
            NextTurn: FnMut(AcceptedInputId) -> TurnId + Send,
        {
            panic!("unused failure transaction")
        }

        async fn reread_failure(
            &mut self,
            _session: SessionId,
            _call: ModelCallId,
            _attachment_failure: Option<AttachmentPreparationFailure>,
        ) -> Result<RetainedPreparedFailureStatus, Self::Error> {
            panic!("unused prepared-failure reread")
        }
    }

    #[derive(Debug)]
    struct FakeFailure {
        errors: VecDeque<FakeError>,
        rereads: VecDeque<Result<RetainedPreparedFailureStatus, FakeError>>,
        calls: usize,
        reread_calls: usize,
    }

    impl FailPreparedModelCallTransaction for FakeFailure {
        type Error = FakeError;

        async fn fail_prepared<NextTurn>(
            &mut self,
            _session: SessionId,
            _call: ModelCallId,
            _cause: PreparedModelCallFailureCause,
            _attachment_failure: Option<AttachmentPreparationFailure>,
            _identities: FailedModelCallTurnIdentities,
            _next_reclassified_turn: NextTurn,
        ) -> Result<FailedModelCallTurn, Self::Error>
        where
            NextTurn: FnMut(AcceptedInputId) -> TurnId + Send,
        {
            self.calls += 1;
            Err(self
                .errors
                .pop_front()
                .expect("one fake failure-commit error"))
        }

        async fn reread_failure(
            &mut self,
            _session: SessionId,
            _call: ModelCallId,
            _attachment_failure: Option<AttachmentPreparationFailure>,
        ) -> Result<RetainedPreparedFailureStatus, Self::Error> {
            self.reread_calls += 1;
            self.rereads
                .pop_front()
                .expect("one fake capability-failure reread")
        }
    }

    /// One `fail_prepared` invocation, as its caller addressed it.
    ///
    /// Recorded rather than discarded: a fake that only counts calls proves
    /// the commit was attempted and nothing about *what* was committed, so a
    /// retry reusing the colliding identities, or a failure written against an
    /// unrelated session, would satisfy the tests below unchanged.
    #[derive(Clone, Debug, Eq, PartialEq)]
    struct FailPreparedCall {
        session: SessionId,
        call: ModelCallId,
        cause: PreparedModelCallFailureCause,
        attachment_failure: Option<AttachmentPreparationFailure>,
        identities: FailedModelCallTurnIdentities,
    }

    #[derive(Debug)]
    struct ScriptedFailure {
        results: VecDeque<Result<FailedModelCallTurn, FakeError>>,
        calls: usize,
        recorded: Vec<FailPreparedCall>,
    }

    impl FailPreparedModelCallTransaction for ScriptedFailure {
        type Error = FakeError;

        async fn fail_prepared<NextTurn>(
            &mut self,
            session: SessionId,
            call: ModelCallId,
            cause: PreparedModelCallFailureCause,
            attachment_failure: Option<AttachmentPreparationFailure>,
            identities: FailedModelCallTurnIdentities,
            _next_reclassified_turn: NextTurn,
        ) -> Result<FailedModelCallTurn, Self::Error>
        where
            NextTurn: FnMut(AcceptedInputId) -> TurnId + Send,
        {
            self.calls += 1;
            self.recorded.push(FailPreparedCall {
                session,
                call,
                cause,
                attachment_failure,
                identities,
            });
            self.results
                .pop_front()
                .expect("one scripted failure-commit result")
        }

        async fn reread_failure(
            &mut self,
            _session: SessionId,
            _call: ModelCallId,
            _attachment_failure: Option<AttachmentPreparationFailure>,
        ) -> Result<RetainedPreparedFailureStatus, Self::Error> {
            panic!("a committed capability failure is never reread")
        }
    }

    #[derive(Debug)]
    struct FakeAuthorization {
        outcomes: VecDeque<Result<AuthorizedModelCall, FakeError>>,
        rereads: VecDeque<Result<ModelCallAuthorizationReread, FakeError>>,
        calls: usize,
        reread_calls: usize,
    }

    impl AuthorizeModelCallTransaction for FakeAuthorization {
        type Error = FakeError;

        async fn authorize(
            &mut self,
            _session: SessionId,
            _call: ModelCallId,
        ) -> Result<AuthorizeModelCallOutcome, Self::Error> {
            self.calls += 1;
            self.outcomes
                .pop_front()
                .expect("one fake authorization outcome")
                .map(|authorized| AuthorizeModelCallOutcome::Authorized(Box::new(authorized)))
        }

        async fn reread_after_ambiguous_commit(
            &mut self,
            _session: SessionId,
            _prepared: &PreparedModelCallRequest,
        ) -> Result<ModelCallAuthorizationReread, Self::Error> {
            self.reread_calls += 1;
            self.rereads
                .pop_front()
                .expect("one fake authorization reread")
        }

        fn cancellation_signal(
            &self,
            _session: SessionId,
            _call: ModelCallId,
        ) -> impl Future<Output = ()> + Send + 'static {
            std::future::pending()
        }
    }

    #[derive(Debug)]
    struct UnusedAuthorization;

    impl AuthorizeModelCallTransaction for UnusedAuthorization {
        type Error = FakeError;

        async fn authorize(
            &mut self,
            _session: SessionId,
            _call: ModelCallId,
        ) -> Result<AuthorizeModelCallOutcome, Self::Error> {
            panic!("unused authorization transaction")
        }

        async fn reread_after_ambiguous_commit(
            &mut self,
            _session: SessionId,
            _prepared: &PreparedModelCallRequest,
        ) -> Result<ModelCallAuthorizationReread, Self::Error> {
            panic!("unused authorization reread")
        }

        fn cancellation_signal(
            &self,
            _session: SessionId,
            _call: ModelCallId,
        ) -> impl Future<Output = ()> + Send + 'static {
            std::future::pending()
        }
    }

    #[derive(Debug)]
    struct NoSendAuthorization {
        calls: usize,
    }

    impl AuthorizeModelCallTransaction for NoSendAuthorization {
        type Error = FakeError;

        async fn authorize(
            &mut self,
            _session: SessionId,
            _call: ModelCallId,
        ) -> Result<AuthorizeModelCallOutcome, Self::Error> {
            self.calls += 1;
            Ok(AuthorizeModelCallOutcome::NoSend)
        }

        async fn reread_after_ambiguous_commit(
            &mut self,
            _session: SessionId,
            _prepared: &PreparedModelCallRequest,
        ) -> Result<ModelCallAuthorizationReread, Self::Error> {
            panic!("a known no-send result needs no reread")
        }

        fn cancellation_signal(
            &self,
            _session: SessionId,
            _call: ModelCallId,
        ) -> impl Future<Output = ()> + Send + 'static {
            std::future::pending()
        }
    }

    #[derive(Debug)]
    struct UnusedObservation;

    impl CommitModelCallObservationTransaction for UnusedObservation {
        type Error = FakeError;

        async fn commit_observation<NextTurn>(
            &mut self,
            _session: SessionId,
            _observation: CorrelatedModelCallTerminalObservation,
            _identities: ModelCallTerminalIdentityCandidates,
            _next_reclassified_turn: NextTurn,
        ) -> Result<Option<ModelCallObservationCommitOutcome>, Self::Error>
        where
            NextTurn: FnMut(AcceptedInputId) -> TurnId + Send,
        {
            panic!("unused observation transaction")
        }

        async fn reread_observation(
            &mut self,
            _session: SessionId,
            _observation: &CorrelatedModelCallTerminalObservation,
        ) -> Result<RetainedModelCallObservationStatus, Self::Error> {
            panic!("unused observation reread")
        }
    }

    #[derive(Debug)]
    struct FakeObservation {
        commit_errors: VecDeque<FakeError>,
        rereads: VecDeque<Result<RetainedModelCallObservationStatus, FakeError>>,
        observed: Vec<CorrelatedModelCallTerminalObservation>,
        commit_calls: usize,
        reread_calls: usize,
    }

    impl CommitModelCallObservationTransaction for FakeObservation {
        type Error = FakeError;

        async fn commit_observation<NextTurn>(
            &mut self,
            _session: SessionId,
            observation: CorrelatedModelCallTerminalObservation,
            _identities: ModelCallTerminalIdentityCandidates,
            _next_reclassified_turn: NextTurn,
        ) -> Result<Option<ModelCallObservationCommitOutcome>, Self::Error>
        where
            NextTurn: FnMut(AcceptedInputId) -> TurnId + Send,
        {
            self.commit_calls += 1;
            self.observed.push(observation);
            Err(self
                .commit_errors
                .pop_front()
                .expect("one fake observation commit failure"))
        }

        async fn reread_observation(
            &mut self,
            _session: SessionId,
            _observation: &CorrelatedModelCallTerminalObservation,
        ) -> Result<RetainedModelCallObservationStatus, Self::Error> {
            self.reread_calls += 1;
            self.rereads
                .pop_front()
                .expect("one fake observation reread")
        }
    }

    #[derive(Debug)]
    struct UnusedProvider;

    impl ModelCallProvider for UnusedProvider {
        type Capability = ();
        type Error = FakeError;

        async fn prepare_capability<Cancellation>(
            &mut self,
            _operation: PreparedModelOperation,
            _cancellation: Cancellation,
        ) -> Result<ModelCallCapabilityPreparation<Self::Capability>, Self::Error>
        where
            Cancellation: Future<Output = ()> + Send + 'static,
        {
            panic!("unused provider capability preparation")
        }

        async fn invoke<AcceptancePossible, Cancellation>(
            &mut self,
            _authorized: AuthorizedModelCall,
            _capability: Self::Capability,
            _acceptance_possible: AcceptancePossible,
            _cancellation: Cancellation,
        ) -> Result<CorrelatedModelCallTerminalObservation, Self::Error>
        where
            AcceptancePossible: FnOnce() + Send,
            Cancellation: Future<Output = ()> + Send + 'static,
        {
            panic!("unused provider interaction")
        }
    }

    #[derive(Debug)]
    struct AttachmentFailureProvider {
        failure: AttachmentPreparationFailure,
        preparation_count: usize,
    }

    impl ModelCallProvider for AttachmentFailureProvider {
        type Capability = ();
        type Error = FakeError;

        async fn prepare_capability<Cancellation>(
            &mut self,
            _operation: PreparedModelOperation,
            _cancellation: Cancellation,
        ) -> Result<ModelCallCapabilityPreparation<Self::Capability>, Self::Error>
        where
            Cancellation: Future<Output = ()> + Send + 'static,
        {
            self.preparation_count += 1;
            Ok(ModelCallCapabilityPreparation::AttachmentFailure(
                self.failure,
            ))
        }

        async fn invoke<AcceptancePossible, Cancellation>(
            &mut self,
            _authorized: AuthorizedModelCall,
            _capability: Self::Capability,
            _acceptance_possible: AcceptancePossible,
            _cancellation: Cancellation,
        ) -> Result<CorrelatedModelCallTerminalObservation, Self::Error>
        where
            AcceptancePossible: FnOnce() + Send,
            Cancellation: Future<Output = ()> + Send + 'static,
        {
            panic!("attachment failure must prevent provider interaction")
        }
    }

    #[derive(Debug)]
    struct BoundaryBlockingProvider {
        crossed: Arc<tokio::sync::Notify>,
        finish: Arc<tokio::sync::Notify>,
        interaction_count: usize,
    }

    impl ModelCallProvider for BoundaryBlockingProvider {
        type Capability = PreparedModelOperation;
        type Error = FakeError;

        async fn prepare_capability<Cancellation>(
            &mut self,
            operation: PreparedModelOperation,
            _cancellation: Cancellation,
        ) -> Result<ModelCallCapabilityPreparation<Self::Capability>, Self::Error>
        where
            Cancellation: Future<Output = ()> + Send + 'static,
        {
            Ok(ModelCallCapabilityPreparation::Ready(operation))
        }

        fn invoke<AcceptancePossible, Cancellation>(
            &mut self,
            _authorized: AuthorizedModelCall,
            _capability: Self::Capability,
            acceptance_possible: AcceptancePossible,
            _cancellation: Cancellation,
        ) -> impl Future<Output = Result<CorrelatedModelCallTerminalObservation, Self::Error>> + Send
        where
            AcceptancePossible: FnOnce() + Send,
            Cancellation: Future<Output = ()> + Send + 'static,
        {
            self.interaction_count += 1;
            let crossed = Arc::clone(&self.crossed);
            let finish = Arc::clone(&self.finish);
            async move {
                acceptance_possible();
                crossed.notify_one();
                finish.notified().await;
                Err(FakeError::Infrastructure)
            }
        }
    }

    fn current_turn_tool_rounds(round_count: u128) -> Vec<ModelConversationMessage> {
        (0..round_count)
            .map(|round| model_tool_use_message(1_000 + round, 2, 2_000 + round, 0))
            .collect()
    }

    fn one_current_batch_with_inherited_tool_history() -> Vec<ModelConversationMessage> {
        (0..32_u32)
            .map(|ordinal| model_tool_use_message(3_000 + u128::from(ordinal), 2, 4_000, ordinal))
            .chain(
                (0..32_u128)
                    .map(|round| model_tool_use_message(5_000 + round, 99, 6_000 + round, 0)),
            )
            .collect()
    }

    /// The user-selected rendering decision: origin input becomes a user-role
    /// message carrying the semantic entry's source, in frontier order.
    #[test]
    fn s02_frontier_rendering_preserves_user_role_order_and_source() {
        let (request, _) = prepared_fixture();
        let credential_reference = credential_reference();
        let operation = PreparedModelOperation::render(
            request,
            credential_reference.clone(),
            None,
            Box::new([]),
            &[],
        )
        .expect("the baseline origin-only frontier renders");
        assert_eq!(operation.credential_reference(), &credential_reference);
        assert_eq!(operation.messages().len(), 1);
        let ModelConversationMessage::User {
            source,
            accepted_input,
            content,
        } = &operation.messages()[0]
        else {
            panic!("an origin entry must render as user content")
        };
        assert_eq!(source.source_session(), identity(1, SessionId::from_uuid));
        assert_eq!(*accepted_input, identity(3, AcceptedInputId::from_uuid));
        assert_eq!(
            content
                .parts()
                .first()
                .expect("the fixture has one provider-visible text part")
                .as_str(),
            "exact user request"
        );
    }

    /// S34: rendering binds the exact optional frozen-epoch system
    /// prompt onto the provider-neutral operation without rewriting it, and
    /// an epoch without a prompt renders none.
    #[test]
    fn s34_render_carries_the_frozen_epoch_system_prompt() {
        let (request, _) = prepared_fixture();
        let prompt = SessionSystemPrompt::try_new(String::from("exact session instructions"))
            .expect("fixture prompt is admissible");

        let prompted = PreparedModelOperation::render(
            request.clone(),
            credential_reference(),
            Some(prompt.clone()),
            Box::new([]),
            &[],
        )
        .expect("the baseline origin-only frontier renders");
        assert_eq!(prompted.system_prompt(), Some(prompt.as_str()));

        let promptless = PreparedModelOperation::render(
            request,
            credential_reference(),
            None,
            Box::new([]),
            &[],
        )
        .expect("the baseline origin-only frontier renders");
        assert_eq!(promptless.system_prompt(), None);
    }

    /// The recorded turn-wide availability bound counts validated producing
    /// calls, not requests or inherited tool history.
    #[test]
    fn s15_automatic_tool_round_bound_counts_current_turn_producing_calls() {
        let current_turn = identity(2, TurnId::from_uuid);
        let below_limit_count = 31;
        let below_limit = current_turn_tool_rounds(below_limit_count);
        assert_eq!(
            automatic_tool_round_count(current_turn, &below_limit),
            below_limit_count as usize
        );

        let at_limit_count = 32;
        let at_limit = current_turn_tool_rounds(at_limit_count);
        assert_eq!(
            automatic_tool_round_count(current_turn, &at_limit),
            at_limit_count as usize
        );

        let one_multi_request_round = one_current_batch_with_inherited_tool_history();
        assert_eq!(
            automatic_tool_round_count(current_turn, &one_multi_request_round),
            1,
            "one current-turn batch and inherited history consume one round",
        );
    }

    /// The seeds of the canonical recorded-override fixture; arbitrary — they
    /// only need to exist as one recorded override.
    const OVERRIDE_COMMAND_SEED: u128 = 81;
    const OVERRIDE_DENIED_REQUEST_SEED: u128 = 82;
    const OVERRIDE_JUDGE_CALL_SEED: u128 = 83;

    /// One recorded override of a denied `guarded` proposal with `{}` arguments
    /// in the canonical fixture session.
    fn recorded_guarded_override() -> signalbox_domain::RecordedUserOverride {
        signalbox_domain::RecordedUserOverride::new(
            identity(
                OVERRIDE_COMMAND_SEED,
                signalbox_domain::DurableCommandId::from_uuid,
            ),
            identity(1, SessionId::from_uuid),
            identity(OVERRIDE_DENIED_REQUEST_SEED, ToolRequestId::from_uuid),
            identity(OVERRIDE_JUDGE_CALL_SEED, ModelCallId::from_uuid),
            signalbox_domain::ToolName::try_new(String::from("guarded"))
                .expect("fixture tool name is valid"),
            signalbox_domain::NormalizedToolArguments::try_from_provider_text(String::from("{}"))
                .expect("fixture arguments are valid"),
        )
    }

    /// One `guarded` proposal with the given provider argument text.
    fn guarded_proposal(arguments: &str) -> AssistantResponsePart {
        AssistantResponsePart::ToolCall(signalbox_domain::ToolCallProposal::new(
            signalbox_domain::ToolName::try_new(String::from("guarded"))
                .expect("fixture tool name is valid"),
            signalbox_domain::NormalizedToolArguments::try_from_provider_text(String::from(
                arguments,
            ))
            .expect("fixture arguments are valid"),
        ))
    }

    /// One completed tool response containing exactly the supplied parts.
    fn completed_with_tools(parts: Vec<AssistantResponsePart>) -> ModelCallTerminalObservation {
        ModelCallTerminalObservation::CompletedWithTools {
            response: signalbox_domain::ToolUsingAssistantResponse::try_from_parts(parts)
                .expect("fixture response contains tools"),
        }
    }

    /// Selects initial approvals for the parts through a service advertising
    /// one `guarded` tool frozen at the given posture.
    #[track_caller]
    fn guarded_tool_approvals(
        posture: signalbox_domain::ToolApprovalPosture,
        parts: Vec<AssistantResponsePart>,
        recorded: &[signalbox_domain::RecordedUserOverride],
    ) -> Box<[InitialToolApproval]> {
        let schema =
            crate::ToolInputSchema::try_new(String::from(r#"{"properties":{},"type":"object"}"#))
                .expect("fixture schema is valid");
        let definition = crate::ToolDefinition::new(
            signalbox_domain::ToolName::try_new(String::from("guarded"))
                .expect("fixture name is valid"),
            String::from("Awaits its frozen approval posture."),
            schema,
            signalbox_domain::ToolPermissionDefault::Confirm,
            signalbox_domain::ToolEffectClass::ExternalEffect,
        )
        .with_approval_posture(posture);
        let catalog = crate::CompiledToolCatalog::try_new([crate::CompiledTool::new(
            definition,
            |_: &signalbox_domain::NormalizedToolArguments| Ok(()),
        )])
        .expect("one tool is unambiguous");
        let service = ModelCallExecutionService::new(
            FixedIds::baseline(),
            FakePrepare {
                outcomes: VecDeque::new(),
                calls: 0,
            },
            UnusedFailure,
            UnusedAuthorization,
            UnusedObservation,
            UnusedProvider,
            InProcessAttemptDispatchGate::default(),
            None,
        )
        .with_tool_catalog(catalog);
        let advertised_tools = service.catalog.definitions();
        service.tool_approvals(
            &completed_with_tools(parts),
            DangerousToolAutoApproval::Disabled,
            &advertised_tools,
            recorded,
        )
    }

    /// S10: a recorded override substitutes for the judge only on the
    /// exact denied command — a proposal with other arguments still parks for
    /// the judge — and the selected approval carries the override command and
    /// the overridden denial.
    #[test]
    fn s10_recorded_override_substitutes_for_the_judge_on_the_exact_command() {
        let recorded = recorded_guarded_override();
        let approvals = guarded_tool_approvals(
            signalbox_domain::ToolApprovalPosture::Delegated,
            vec![
                guarded_proposal("{}"),
                guarded_proposal(r#"{"timezone":"UTC"}"#),
            ],
            std::slice::from_ref(&recorded),
        );

        assert_eq!(
            approvals.as_ref(),
            [
                InitialToolApproval::UserOverride {
                    command: recorded.command(),
                    denied_request: recorded.denied_request(),
                },
                InitialToolApproval::Delegated,
            ]
        );
    }

    /// S10: one recorded override pre-approves at most one proposal
    /// per response; a second identical proposal parks for the judge again.
    #[test]
    fn s10_recorded_override_is_consumed_at_most_once_per_response() {
        let recorded = recorded_guarded_override();
        let approvals = guarded_tool_approvals(
            signalbox_domain::ToolApprovalPosture::Delegated,
            vec![guarded_proposal("{}"), guarded_proposal("{}")],
            std::slice::from_ref(&recorded),
        );

        assert_eq!(
            approvals.as_ref(),
            [
                InitialToolApproval::UserOverride {
                    command: recorded.command(),
                    denied_request: recorded.denied_request(),
                },
                InitialToolApproval::Delegated,
            ]
        );
    }

    /// S10: a recorded override substitutes only where the judge
    /// would decide; a human-frozen selection is never overridden.
    #[test]
    fn s10_recorded_override_never_bypasses_a_human_selection() {
        let approvals = guarded_tool_approvals(
            signalbox_domain::ToolApprovalPosture::Human,
            vec![guarded_proposal("{}")],
            &[recorded_guarded_override()],
        );

        assert_eq!(approvals.as_ref(), [InitialToolApproval::Human]);
    }

    /// S10: one identity is minted per ordered response
    /// part/request, approval stays pinned to the advertised catalog snapshot,
    /// mixed auto/confirm policy parks without a continuation attempt, and the
    /// adapter still receives a stopped race closure.
    #[test]
    fn s10_tool_response_candidates_preserve_order_and_policy() {
        let schema =
            crate::ToolInputSchema::try_new(String::from(r#"{"properties":{},"type":"object"}"#))
                .expect("fixture schema is valid");
        let definition = crate::ToolDefinition::new(
            signalbox_domain::ToolName::try_new(String::from("automatic"))
                .expect("fixture name is valid"),
            String::from("Runs automatically."),
            schema,
            signalbox_domain::ToolPermissionDefault::Auto,
            signalbox_domain::ToolEffectClass::EffectFree,
        );
        let catalog = crate::CompiledToolCatalog::try_new([crate::CompiledTool::new(
            definition,
            |_: &signalbox_domain::NormalizedToolArguments| Ok(()),
        )])
        .expect("one tool is unambiguous");
        let mut service = ModelCallExecutionService::new(
            FixedIds::baseline(),
            FakePrepare {
                outcomes: VecDeque::new(),
                calls: 0,
            },
            UnusedFailure,
            UnusedAuthorization,
            UnusedObservation,
            UnusedProvider,
            InProcessAttemptDispatchGate::default(),
            None,
        )
        .with_tool_catalog(catalog);
        let observation = tool_response();
        let advertised_tools = service.catalog.definitions();
        service.catalog = Arc::new(NoToolCatalog);
        let approvals = service.tool_approvals(
            &observation,
            DangerousToolAutoApproval::Disabled,
            &advertised_tools,
            &[],
        );
        assert_eq!(
            approvals.as_ref(),
            [
                InitialToolApproval::PolicyAuto,
                InitialToolApproval::Confirm
            ]
        );

        let ModelCallTerminalIdentityCandidates::ToolRound {
            continuing,
            stopped,
        } = service.next_terminal_identities(&observation, &approvals)
        else {
            panic!("tool response requires both race-safe closures");
        };
        assert_eq!(continuing.response_parts().len(), 3);
        assert_eq!(continuing.continuation_attempt(), None);
        let [
            ToolResponsePartIdentity::Text { .. },
            ToolResponsePartIdentity::ToolCall {
                approval: first_approval,
                ..
            },
            ToolResponsePartIdentity::ToolCall {
                approval: second_approval,
                ..
            },
        ] = continuing.response_parts()
        else {
            panic!("fixture response preserves one text part then two tool calls");
        };
        assert_eq!(*first_approval, InitialToolApproval::PolicyAuto);
        assert_eq!(*second_approval, InitialToolApproval::Confirm);

        let non_overridable_approvals = [
            InitialToolApproval::PolicyAuto,
            InitialToolApproval::AlwaysConfirm,
        ];
        service.ids = FixedIds::baseline();
        let ModelCallTerminalIdentityCandidates::ToolRound { continuing, .. } =
            service.next_terminal_identities(&observation, &non_overridable_approvals)
        else {
            panic!("tool response requires both race-safe closures");
        };
        assert_eq!(continuing.continuation_attempt(), None);
        assert_eq!(
            stopped,
            StoppedToolRoundModelCallIdentities::new(
                vec![
                    StoppedToolResponsePartIdentity::text(identity(
                        33,
                        SemanticTranscriptEntryId::from_uuid,
                    )),
                    StoppedToolResponsePartIdentity::tool_call(
                        identity(34, SemanticTranscriptEntryId::from_uuid),
                        identity(62, ToolRequestId::from_uuid),
                        identity(35, SemanticTranscriptEntryId::from_uuid),
                        InitialToolApproval::PolicyAuto,
                    ),
                    StoppedToolResponsePartIdentity::tool_call(
                        identity(36, SemanticTranscriptEntryId::from_uuid),
                        identity(63, ToolRequestId::from_uuid),
                        identity(37, SemanticTranscriptEntryId::from_uuid),
                        InitialToolApproval::Confirm,
                    ),
                ],
                identity(38, SemanticTranscriptEntryId::from_uuid),
                identity(41, ContextFrontierId::from_uuid),
            ),
            "lifecycle-dependent candidates receive a disjoint identity inventory"
        );
    }

    /// S10: a credential-suppressed proposal bypasses the
    /// advertised execution policy and receives an automatic safety denial.
    #[test]
    fn s10_suppressed_proposal_forces_runtime_safety_denial() {
        let service = ModelCallExecutionService::new(
            FixedIds::baseline(),
            FakePrepare {
                outcomes: VecDeque::new(),
                calls: 0,
            },
            UnusedFailure,
            UnusedAuthorization,
            UnusedObservation,
            UnusedProvider,
            InProcessAttemptDispatchGate::default(),
            None,
        );
        let response = signalbox_domain::ToolUsingAssistantResponse::try_from_parts(vec![
            signalbox_domain::AssistantResponsePart::ToolCall(
                signalbox_domain::ToolCallProposal::suppressed(
                    signalbox_domain::ToolName::try_new(String::from("sandboxed_exec"))
                        .expect("fixture tool name is valid"),
                ),
            ),
        ])
        .expect("suppressed proposal remains one bounded logical request");
        let observation = ModelCallTerminalObservation::CompletedWithTools { response };

        assert_eq!(
            service
                .tool_approvals(
                    &observation,
                    DangerousToolAutoApproval::ApproveAll,
                    &[],
                    &[]
                )
                .as_ref(),
            [InitialToolApproval::RuntimeSafetyDeny]
        );
    }
    /// S28: attested imported text keeps its exact
    /// source-attested role, semantic source, imported authority, and decoded
    /// text without acquiring a native input or call identity.
    #[test]
    fn s28_frontier_rendering_preserves_imported_text_roles_and_sources() {
        let imported_user_entry =
            identity(110, signalbox_domain::ImportedTranscriptEntryId::from_uuid);
        let imported_assistant_entry =
            identity(111, signalbox_domain::ImportedTranscriptEntryId::from_uuid);
        let projected_user = SemanticTranscriptEntryRef::from_source(
            identity(112, SessionId::from_uuid),
            identity(113, SemanticTranscriptEntryId::from_uuid),
        );
        let projected_assistant = SemanticTranscriptEntryRef::from_source(
            identity(114, SessionId::from_uuid),
            identity(115, SemanticTranscriptEntryId::from_uuid),
        );
        let exact_user = ImportedText::new(String::from(" \timported\0user\r\n"));
        let exact_assistant = ImportedText::new(String::new());
        let entries = [
            (
                projected_user,
                SemanticTranscriptEntryPayload::Imported {
                    imported_entry: imported_user_entry,
                    source_speaker: ImportedSourceAttestation::Attested(ImportedSpeaker::User),
                    content: ImportedTranscriptContent::Text(ImportedSourceAttestation::Attested(
                        exact_user.clone(),
                    )),
                },
            ),
            (
                projected_assistant,
                SemanticTranscriptEntryPayload::Imported {
                    imported_entry: imported_assistant_entry,
                    source_speaker: ImportedSourceAttestation::Attested(ImportedSpeaker::Assistant),
                    content: ImportedTranscriptContent::Text(ImportedSourceAttestation::Attested(
                        exact_assistant.clone(),
                    )),
                },
            ),
        ];

        let messages = render_frontier_messages(
            entries.iter().map(|(source, payload)| (*source, payload)),
            |_| panic!("imported text must not request native accepted-input content"),
            |_| panic!("imported text must not request attachment facts"),
            std::iter::empty(),
        )
        .expect("attested imported text is conservatively renderable");

        assert_eq!(messages.len(), 2);
        let ModelConversationMessage::ImportedUser {
            source,
            imported_entry,
            content,
        } = &messages[0]
        else {
            panic!("attested imported user text must retain the imported user role")
        };
        assert_eq!(*source, projected_user);
        assert_eq!(*imported_entry, imported_user_entry);
        assert_eq!(content.as_str(), exact_user.as_str());
        let ModelConversationMessage::ImportedAssistant {
            source,
            imported_entry,
            content,
        } = &messages[1]
        else {
            panic!("attested imported assistant text must retain the imported assistant role")
        };
        assert_eq!(*source, projected_assistant);
        assert_eq!(*imported_entry, imported_assistant_entry);
        assert_eq!(content.as_str(), exact_assistant.as_str());
    }

    /// S28: typed imported text or speaker absence remains
    /// model-invisible rather than guessing a role or fabricating content.
    #[test]
    fn s28_frontier_rendering_skips_imported_text_with_typed_absence() {
        let projected_session = identity(120, SessionId::from_uuid);
        let imported_entry = identity(121, signalbox_domain::ImportedTranscriptEntryId::from_uuid);
        let exact_text = ImportedText::new(String::from("must remain hidden"));
        let entries = [
            (
                SemanticTranscriptEntryRef::from_source(
                    projected_session,
                    identity(122, SemanticTranscriptEntryId::from_uuid),
                ),
                SemanticTranscriptEntryPayload::Imported {
                    imported_entry,
                    source_speaker: ImportedSourceAttestation::Attested(ImportedSpeaker::User),
                    content: ImportedTranscriptContent::Text(
                        ImportedSourceAttestation::AttestedAbsent,
                    ),
                },
            ),
            (
                SemanticTranscriptEntryRef::from_source(
                    projected_session,
                    identity(123, SemanticTranscriptEntryId::from_uuid),
                ),
                SemanticTranscriptEntryPayload::Imported {
                    imported_entry,
                    source_speaker: ImportedSourceAttestation::Attested(ImportedSpeaker::Assistant),
                    content: ImportedTranscriptContent::Text(
                        ImportedSourceAttestation::NotAttested,
                    ),
                },
            ),
            (
                SemanticTranscriptEntryRef::from_source(
                    projected_session,
                    identity(124, SemanticTranscriptEntryId::from_uuid),
                ),
                SemanticTranscriptEntryPayload::Imported {
                    imported_entry,
                    source_speaker: ImportedSourceAttestation::AttestedAbsent,
                    content: ImportedTranscriptContent::Text(ImportedSourceAttestation::Attested(
                        exact_text.clone(),
                    )),
                },
            ),
            (
                SemanticTranscriptEntryRef::from_source(
                    projected_session,
                    identity(125, SemanticTranscriptEntryId::from_uuid),
                ),
                SemanticTranscriptEntryPayload::Imported {
                    imported_entry,
                    source_speaker: ImportedSourceAttestation::NotAttested,
                    content: ImportedTranscriptContent::Text(ImportedSourceAttestation::Attested(
                        exact_text,
                    )),
                },
            ),
        ];

        let messages = render_frontier_messages(
            entries.iter().map(|(source, payload)| (*source, payload)),
            |_| panic!("imported absence must not request native accepted-input content"),
            |_| panic!("imported absence must not request attachment facts"),
            std::iter::empty(),
        )
        .expect("typed imported absence is conservatively skipped");

        assert!(
            messages.is_empty(),
            "typed absence cannot become provider-visible content"
        );
    }

    /// S28: the conservative frontier renderer leaves
    /// every imported non-text vocabulary member model-invisible without
    /// removing it from the semantic frontier or inventing native tool facts.
    #[test]
    fn s28_frontier_rendering_skips_every_imported_non_text_variant() {
        let projected_session = identity(130, SessionId::from_uuid);
        let imported_entry = identity(131, signalbox_domain::ImportedTranscriptEntryId::from_uuid);
        let speaker = ImportedSourceAttestation::Attested(ImportedSpeaker::User);
        let entries = [
            (
                SemanticTranscriptEntryRef::from_source(
                    projected_session,
                    identity(132, SemanticTranscriptEntryId::from_uuid),
                ),
                SemanticTranscriptEntryPayload::Imported {
                    imported_entry,
                    source_speaker: speaker.clone(),
                    content: ImportedTranscriptContent::SourceEvent {
                        source_type: ImportedSourceAttestation::NotAttested,
                    },
                },
            ),
            (
                SemanticTranscriptEntryRef::from_source(
                    projected_session,
                    identity(133, SemanticTranscriptEntryId::from_uuid),
                ),
                SemanticTranscriptEntryPayload::Imported {
                    imported_entry,
                    source_speaker: speaker.clone(),
                    content: ImportedTranscriptContent::SourceMessageBlock {
                        source_type: ImportedSourceAttestation::NotAttested,
                    },
                },
            ),
            (
                SemanticTranscriptEntryRef::from_source(
                    projected_session,
                    identity(134, SemanticTranscriptEntryId::from_uuid),
                ),
                SemanticTranscriptEntryPayload::Imported {
                    imported_entry,
                    source_speaker: speaker.clone(),
                    content: ImportedTranscriptContent::ToolCall {
                        source_call_id: ImportedSourceAttestation::NotAttested,
                        name: ImportedSourceAttestation::NotAttested,
                        input: ImportedSourceAttestation::NotAttested,
                        caller: ImportedSourceAttestation::NotAttested,
                    },
                },
            ),
            (
                SemanticTranscriptEntryRef::from_source(
                    projected_session,
                    identity(135, SemanticTranscriptEntryId::from_uuid),
                ),
                SemanticTranscriptEntryPayload::Imported {
                    imported_entry,
                    source_speaker: speaker.clone(),
                    content: ImportedTranscriptContent::ToolResult {
                        source_call_id: ImportedSourceAttestation::NotAttested,
                        content: ImportedSourceAttestation::NotAttested,
                        is_error: ImportedSourceAttestation::NotAttested,
                    },
                },
            ),
            (
                SemanticTranscriptEntryRef::from_source(
                    projected_session,
                    identity(136, SemanticTranscriptEntryId::from_uuid),
                ),
                SemanticTranscriptEntryPayload::Imported {
                    imported_entry,
                    source_speaker: speaker.clone(),
                    content: ImportedTranscriptContent::Thinking {
                        thinking: ImportedSourceAttestation::NotAttested,
                        signature: ImportedSourceAttestation::NotAttested,
                    },
                },
            ),
            (
                SemanticTranscriptEntryRef::from_source(
                    projected_session,
                    identity(137, SemanticTranscriptEntryId::from_uuid),
                ),
                SemanticTranscriptEntryPayload::Imported {
                    imported_entry,
                    source_speaker: speaker.clone(),
                    content: ImportedTranscriptContent::RedactedThinking {
                        data: ImportedSourceAttestation::NotAttested,
                    },
                },
            ),
            (
                SemanticTranscriptEntryRef::from_source(
                    projected_session,
                    identity(138, SemanticTranscriptEntryId::from_uuid),
                ),
                SemanticTranscriptEntryPayload::Imported {
                    imported_entry,
                    source_speaker: speaker.clone(),
                    content: ImportedTranscriptContent::Document {
                        source: ImportedSourceAttestation::NotAttested,
                    },
                },
            ),
            (
                SemanticTranscriptEntryRef::from_source(
                    projected_session,
                    identity(139, SemanticTranscriptEntryId::from_uuid),
                ),
                SemanticTranscriptEntryPayload::Imported {
                    imported_entry,
                    source_speaker: speaker,
                    content: ImportedTranscriptContent::MessageContentAbsent(
                        ImportedMessageContentAbsence::EmptyBlockArray,
                    ),
                },
            ),
        ];

        let messages = render_frontier_messages(
            entries.iter().map(|(source, payload)| (*source, payload)),
            |_| panic!("imported non-text must not request native accepted-input content"),
            |_| panic!("imported non-text must not request attachment facts"),
            std::iter::empty(),
        )
        .expect("imported non-text is conservatively skipped");

        assert!(
            messages.is_empty(),
            "non-text imported history cannot become a native provider message"
        );
    }

    /// S02: mixed semantic content keeps exact role order and
    /// source-qualified provenance, including entries created by a different
    /// session; terminal markers do not invent provider-visible messages.
    #[test]
    fn s02_frontier_rendering_preserves_mixed_roles_and_inherited_sources() {
        let inherited_session = identity(90, SessionId::from_uuid);
        let current_session = identity(1, SessionId::from_uuid);
        let inherited_input = identity(91, AcceptedInputId::from_uuid);
        let current_input = identity(92, AcceptedInputId::from_uuid);
        let failed_input = identity(99, AcceptedInputId::from_uuid);
        let producing_call = identity(93, ModelCallId::from_uuid);
        let inherited_content =
            UserContent::try_text(String::from("inherited user request")).expect("valid text");
        let current_content =
            UserContent::try_text(String::from("current user request")).expect("valid text");
        let failed_content =
            UserContent::try_text(String::from("failed user request")).expect("valid text");
        let assistant_text = AssistantText::try_new(String::from("inherited assistant reply"))
            .expect("valid assistant text");
        let origin_contents = std::collections::HashMap::from([
            (inherited_input, inherited_content.clone()),
            (current_input, current_content.clone()),
            (failed_input, failed_content.clone()),
        ]);
        let entries = [
            (
                SemanticTranscriptEntryRef::from_source(
                    inherited_session,
                    identity(94, SemanticTranscriptEntryId::from_uuid),
                ),
                SemanticTranscriptEntryPayload::OriginAcceptedInput {
                    accepted_input: inherited_input,
                },
            ),
            (
                SemanticTranscriptEntryRef::from_source(
                    inherited_session,
                    identity(95, SemanticTranscriptEntryId::from_uuid),
                ),
                SemanticTranscriptEntryPayload::AssistantText {
                    producing_call,
                    value: assistant_text.clone(),
                },
            ),
            (
                SemanticTranscriptEntryRef::from_source(
                    inherited_session,
                    identity(96, SemanticTranscriptEntryId::from_uuid),
                ),
                SemanticTranscriptEntryPayload::TurnCompleted {
                    turn: identity(97, TurnId::from_uuid),
                },
            ),
            (
                SemanticTranscriptEntryRef::from_source(
                    current_session,
                    identity(98, SemanticTranscriptEntryId::from_uuid),
                ),
                SemanticTranscriptEntryPayload::OriginAcceptedInput {
                    accepted_input: failed_input,
                },
            ),
            (
                SemanticTranscriptEntryRef::from_source(
                    current_session,
                    identity(100, SemanticTranscriptEntryId::from_uuid),
                ),
                SemanticTranscriptEntryPayload::TurnFailed {
                    turn: identity(101, TurnId::from_uuid),
                },
            ),
            (
                SemanticTranscriptEntryRef::from_source(
                    current_session,
                    identity(102, SemanticTranscriptEntryId::from_uuid),
                ),
                SemanticTranscriptEntryPayload::OriginAcceptedInput {
                    accepted_input: current_input,
                },
            ),
        ];

        let messages = render_frontier_messages(
            entries.iter().map(|(source, payload)| (*source, payload)),
            |accepted_input| origin_contents.get(&accepted_input).cloned(),
            |_| None,
            [],
        )
        .expect("the admitted mixed text frontier renders");

        expect![[r#"
            [
                User {
                    source: SemanticTranscriptEntryRef {
                        source_session: SessionId(
                            00000000-0000-0000-0000-00000000005a,
                        ),
                        entry: SemanticTranscriptEntryId(
                            00000000-0000-0000-0000-00000000005e,
                        ),
                    },
                    accepted_input: AcceptedInputId(
                        00000000-0000-0000-0000-00000000005b,
                    ),
                    content: ModelUserContent {
                        parts: [
                            Text(
                                NonEmptyUnicodeText(<redacted>),
                            ),
                        ],
                    },
                },
                Assistant {
                    source: SemanticTranscriptEntryRef {
                        source_session: SessionId(
                            00000000-0000-0000-0000-00000000005a,
                        ),
                        entry: SemanticTranscriptEntryId(
                            00000000-0000-0000-0000-00000000005f,
                        ),
                    },
                    producing_call: ModelCallId(
                        00000000-0000-0000-0000-00000000005d,
                    ),
                    content: AssistantText(
                        NonEmptyUnicodeText(<redacted>),
                    ),
                },
                User {
                    source: SemanticTranscriptEntryRef {
                        source_session: SessionId(
                            00000000-0000-0000-0000-000000000001,
                        ),
                        entry: SemanticTranscriptEntryId(
                            00000000-0000-0000-0000-000000000062,
                        ),
                    },
                    accepted_input: AcceptedInputId(
                        00000000-0000-0000-0000-000000000063,
                    ),
                    content: ModelUserContent {
                        parts: [
                            Text(
                                NonEmptyUnicodeText(<redacted>),
                            ),
                        ],
                    },
                },
                User {
                    source: SemanticTranscriptEntryRef {
                        source_session: SessionId(
                            00000000-0000-0000-0000-000000000001,
                        ),
                        entry: SemanticTranscriptEntryId(
                            00000000-0000-0000-0000-000000000066,
                        ),
                    },
                    accepted_input: AcceptedInputId(
                        00000000-0000-0000-0000-00000000005c,
                    ),
                    content: ModelUserContent {
                        parts: [
                            Text(
                                NonEmptyUnicodeText(<redacted>),
                            ),
                        ],
                    },
                },
            ]
        "#]]
        .assert_debug_eq(&messages);
        assert_eq!(
            &messages[0],
            &ModelConversationMessage::User {
                source: entries[0].0,
                accepted_input: inherited_input,
                content: rendered_text(inherited_content),
            }
        );
        assert_eq!(
            &messages[1],
            &ModelConversationMessage::Assistant {
                source: entries[1].0,
                producing_call,
                content: assistant_text,
            }
        );
        assert_eq!(
            &messages[2],
            &ModelConversationMessage::User {
                source: entries[3].0,
                accepted_input: failed_input,
                content: rendered_text(failed_content),
            }
        );
        assert_eq!(
            &messages[3],
            &ModelConversationMessage::User {
                source: entries[5].0,
                accepted_input: current_input,
                content: rendered_text(current_content),
            }
        );
    }

    /// S02: durable request, attempt, and denial authority renders
    /// reference-only tool semantics into their exact provider-visible roles
    /// without changing source order.
    #[test]
    fn s02_frontier_rendering_resolves_exact_tool_roles_in_source_order() {
        let completed_request = model_tool_request(0);
        let denied_request = model_tool_request(1);
        let closed_request = model_tool_request(2);
        let completed_use_source = SemanticTranscriptEntryRef::from_source(
            completed_request.session(),
            identity(110, SemanticTranscriptEntryId::from_uuid),
        );
        let completed_result_source = SemanticTranscriptEntryRef::from_source(
            completed_request.session(),
            identity(111, SemanticTranscriptEntryId::from_uuid),
        );
        let denied_use_source = SemanticTranscriptEntryRef::from_source(
            denied_request.session(),
            identity(112, SemanticTranscriptEntryId::from_uuid),
        );
        let denied_result_source = SemanticTranscriptEntryRef::from_source(
            denied_request.session(),
            identity(113, SemanticTranscriptEntryId::from_uuid),
        );
        let closed_use_source = SemanticTranscriptEntryRef::from_source(
            closed_request.session(),
            identity(114, SemanticTranscriptEntryId::from_uuid),
        );
        let closed_result_source = SemanticTranscriptEntryRef::from_source(
            closed_request.session(),
            identity(115, SemanticTranscriptEntryId::from_uuid),
        );
        let completed_result = ToolResultContent::Text(
            ToolResultText::try_new(String::from(r#"{"timezone":"UTC"}"#))
                .expect("fixture result is valid"),
        );
        let attempt_id = identity(116, signalbox_domain::ToolAttemptId::from_uuid);
        let signalbox_domain::ReconstitutedToolAttempt::Ended(completed_attempt) =
            ToolAttemptReconstitutionInput::new(
                attempt_id,
                completed_request.id(),
                completed_request.session(),
                completed_request.turn(),
                identity(117, TurnAttemptId::from_uuid),
                ToolEffectClass::EffectFree,
                ToolDispatchGeneration::first(),
                ToolAttemptReconstitutionState::Ended(ToolAttemptEnd::Completed {
                    result: completed_result.clone(),
                }),
            )
            .reconstitute()
            .expect("the first tool dispatch generation is supported")
        else {
            panic!("terminal fixture reconstitutes as ended")
        };
        let denial_reason = ToolDenialReason::try_new(String::from("user declined"))
            .expect("fixture denial reason is valid");
        let denial_command = DecideToolRequest::try_new(
            identity(118, DurableCommandId::from_uuid),
            denied_request.id(),
            ToolApprovalDecision::Deny {
                reason: Some(denial_reason.clone()),
            },
        )
        .expect("the fixture command identity is admitted")
        .prepare_applied(&denied_request)
        .expect("the command names the exact request");
        let denial = ToolApprovalResolutionReconstitutionInput::user_command(denial_command)
            .reconstitute()
            .expect("user denial provenance is implemented");
        let entries = [
            (
                completed_use_source,
                SemanticTranscriptEntryPayload::AssistantToolUse {
                    producing_call: completed_request.producing_call(),
                    request: completed_request.id(),
                },
            ),
            (
                completed_result_source,
                SemanticTranscriptEntryPayload::ToolExecutionResult {
                    attempt: attempt_id,
                },
            ),
            (
                denied_use_source,
                SemanticTranscriptEntryPayload::AssistantToolUse {
                    producing_call: denied_request.producing_call(),
                    request: denied_request.id(),
                },
            ),
            (
                denied_result_source,
                SemanticTranscriptEntryPayload::ToolDenied {
                    request: denied_request.id(),
                },
            ),
            (
                closed_use_source,
                SemanticTranscriptEntryPayload::AssistantToolUse {
                    producing_call: closed_request.producing_call(),
                    request: closed_request.id(),
                },
            ),
            (
                closed_result_source,
                SemanticTranscriptEntryPayload::ToolClosed {
                    request: closed_request.id(),
                },
            ),
        ];
        let evidence = [
            ResolvedToolConversationEntry::AssistantToolUse {
                source: completed_use_source,
                request: completed_request.clone(),
            },
            ResolvedToolConversationEntry::ExecutionResult {
                source: completed_result_source,
                request: completed_request.clone(),
                attempt: completed_attempt,
            },
            ResolvedToolConversationEntry::AssistantToolUse {
                source: denied_use_source,
                request: denied_request.clone(),
            },
            ResolvedToolConversationEntry::Denied {
                source: denied_result_source,
                request: denied_request.clone(),
                approval: denial,
            },
            ResolvedToolConversationEntry::AssistantToolUse {
                source: closed_use_source,
                request: closed_request.clone(),
            },
            ResolvedToolConversationEntry::Closed {
                source: closed_result_source,
                request: closed_request.clone(),
            },
        ];

        let messages = render_frontier_messages(
            entries.iter().map(|(source, payload)| (*source, payload)),
            |_| None,
            |_| None,
            evidence.iter(),
        )
        .expect("exact tool evidence renders");

        assert_eq!(
            messages.as_ref(),
            [
                ModelConversationMessage::AssistantToolUse {
                    source: completed_use_source,
                    producing_call: completed_request.producing_call(),
                    request: completed_request.clone(),
                },
                ModelConversationMessage::ToolResult {
                    source: completed_result_source,
                    request: completed_request.id(),
                    content: ModelToolResultContent::Success(completed_result),
                },
                ModelConversationMessage::AssistantToolUse {
                    source: denied_use_source,
                    producing_call: denied_request.producing_call(),
                    request: denied_request.clone(),
                },
                ModelConversationMessage::ToolResult {
                    source: denied_result_source,
                    request: denied_request.id(),
                    content: ModelToolResultContent::Denied {
                        reason: Some(denial_reason),
                    },
                },
                ModelConversationMessage::AssistantToolUse {
                    source: closed_use_source,
                    producing_call: closed_request.producing_call(),
                    request: closed_request.clone(),
                },
                ModelConversationMessage::ToolResult {
                    source: closed_result_source,
                    request: closed_request.id(),
                    content: ModelToolResultContent::ClosedByTurnEnd,
                },
            ]
        );
    }

    /// S02: a terminal attempt from another turn cannot supply
    /// authority for a tool-result semantic entry.
    #[test]
    fn s02_frontier_rendering_rejects_cross_turn_tool_result_evidence() {
        let request = model_tool_request(0);
        let source = SemanticTranscriptEntryRef::from_source(
            request.session(),
            identity(120, SemanticTranscriptEntryId::from_uuid),
        );
        let attempt_id = identity(121, signalbox_domain::ToolAttemptId::from_uuid);
        let signalbox_domain::ReconstitutedToolAttempt::Ended(cross_turn_attempt) =
            ToolAttemptReconstitutionInput::new(
                attempt_id,
                request.id(),
                request.session(),
                identity(122, TurnId::from_uuid),
                identity(123, TurnAttemptId::from_uuid),
                ToolEffectClass::EffectFree,
                ToolDispatchGeneration::first(),
                ToolAttemptReconstitutionState::Ended(ToolAttemptEnd::Completed {
                    result: ToolResultContent::Text(
                        ToolResultText::try_new(String::from("cross-wired"))
                            .expect("fixture result is valid"),
                    ),
                }),
            )
            .reconstitute()
            .expect("the first tool dispatch generation is supported")
        else {
            panic!("terminal fixture reconstitutes as ended")
        };
        let payload = SemanticTranscriptEntryPayload::ToolExecutionResult {
            attempt: attempt_id,
        };
        let evidence = ResolvedToolConversationEntry::ExecutionResult {
            source,
            request,
            attempt: cross_turn_attempt,
        };

        let error = render_frontier_messages([(source, &payload)], |_| None, |_| None, [&evidence])
            .expect_err("cross-turn tool evidence must fail closed");

        assert_eq!(
            error,
            ModelFrontierRenderingError::MissingOrMismatchedToolEvidence { entry: source }
        );
    }

    /// S02: a newly committed Prepared checkpoint ends
    /// the invocation before capability preparation or authorization.
    #[tokio::test]
    async fn s02_checkpoint_stops_before_every_later_port() {
        let checkpoint = identity(70, ModelCallId::from_uuid);
        let mut service = ModelCallExecutionService::new(
            FixedIds::baseline(),
            FakePrepare {
                outcomes: [Ok(PrepareModelCallOutcome::Checkpointed(checkpoint))].into(),
                calls: 0,
            },
            UnusedFailure,
            UnusedAuthorization,
            UnusedObservation,
            UnusedProvider,
            InProcessAttemptDispatchGate::default(),
            None,
        );
        assert_eq!(
            service
                .execute(identity(1, SessionId::from_uuid))
                .await
                .expect("checkpointing succeeds"),
            ModelCallExecutionOutcome::Checkpointed(checkpoint)
        );
        let (_, prepare, ..) = service.into_parts();
        assert_eq!(prepare.calls, 1);
    }

    /// S02: a proven fresh-identity collision retries only the
    /// rolled-back prepare transaction with fresh candidates.
    #[tokio::test]
    async fn s02_prepare_identity_collision_retries_transaction_only() {
        let checkpoint = identity(71, ModelCallId::from_uuid);
        let mut service = ModelCallExecutionService::new(
            FixedIds::baseline(),
            FakePrepare {
                outcomes: [
                    Err(FakeError::IdentityCollision),
                    Ok(PrepareModelCallOutcome::Checkpointed(checkpoint)),
                ]
                .into(),
                calls: 0,
            },
            UnusedFailure,
            UnusedAuthorization,
            UnusedObservation,
            UnusedProvider,
            InProcessAttemptDispatchGate::default(),
            None,
        );
        assert_eq!(
            service
                .execute(identity(1, SessionId::from_uuid))
                .await
                .expect("proven collision is retryable"),
            ModelCallExecutionOutcome::Checkpointed(checkpoint)
        );
        let (_, prepare, ..) = service.into_parts();
        assert_eq!(prepare.calls, 2);
    }

    /// durable cancellation during capability preparation is
    /// authoritative no-work, not a local capability failure to terminalize.
    #[tokio::test]
    async fn capability_preparation_cancellation_stops_without_failure_commit() {
        let (request, _) = prepared_fixture();
        let session = request.session();
        let mut service = ModelCallExecutionService::new(
            FixedIds::baseline(),
            FakePrepare {
                outcomes: [Ok(ready(request))].into(),
                calls: 0,
            },
            UnusedFailure,
            UnusedAuthorization,
            UnusedObservation,
            ScriptedModelCallProvider::new([ScriptedModelCallStep::CapabilityCancelled]),
            InProcessAttemptDispatchGate::default(),
            None,
        );

        assert_eq!(
            service
                .execute(session)
                .await
                .expect("durable cancellation is authoritative"),
            ModelCallExecutionOutcome::NoWork
        );
        let (_, prepare, _, _, _, provider, ..) = service.into_parts();
        assert_eq!(prepare.calls, 1);
        assert_eq!(provider.capability_preparation_count(), 1);
    }

    #[tokio::test]
    async fn prepared_capability_receives_the_configured_tool_catalog_snapshot() {
        let (request, _) = prepared_fixture();
        let session = request.session();
        let definition = crate::ToolDefinition::new(
            ToolName::try_new(String::from("current_time")).expect("fixture tool name"),
            String::from("Returns the current UTC time."),
            crate::ToolInputSchema::try_new(String::from(
                r#"{"additionalProperties":false,"properties":{},"type":"object"}"#,
            ))
            .expect("fixture schema"),
            ToolPermissionDefault::Auto,
            ToolEffectClass::EffectFree,
        );
        let catalog = crate::CompiledToolCatalog::try_new([crate::CompiledTool::new(
            definition.clone(),
            |_arguments: &NormalizedToolArguments| Ok(()),
        )])
        .expect("fixture catalog");
        let mut service = ModelCallExecutionService::new(
            FixedIds::baseline(),
            FakePrepare {
                outcomes: [Ok(ready(request))].into(),
                calls: 0,
            },
            UnusedFailure,
            UnusedAuthorization,
            UnusedObservation,
            ScriptedModelCallProvider::new([ScriptedModelCallStep::CapabilityCancelled]),
            InProcessAttemptDispatchGate::default(),
            None,
        )
        .with_tool_catalog(catalog);

        assert_eq!(
            service
                .execute(session)
                .await
                .expect("durable cancellation is authoritative"),
            ModelCallExecutionOutcome::NoWork
        );
        let (_, _, _, _, _, provider, ..) = service.into_parts();
        assert_eq!(
            provider.last_prepared_tools(),
            Some([definition].as_slice())
        );
    }

    /// S34: the execution loop presents the prepare transaction's
    /// exact frozen-epoch system prompt to the provider port with the
    /// capability operation; a promptless epoch presents none.
    #[tokio::test]
    async fn s34_prepared_capability_receives_the_frozen_epoch_system_prompt() {
        let (request, _) = prepared_fixture();
        let session = request.session();
        let prompt = SessionSystemPrompt::try_new(String::from("exact session instructions"))
            .expect("fixture prompt is admissible");
        let mut service = ModelCallExecutionService::new(
            FixedIds::baseline(),
            FakePrepare {
                outcomes: [Ok(PrepareModelCallOutcome::Ready {
                    request: Box::new(request.clone()),
                    credential_reference: credential_reference(),
                    dangerous_tool_auto_approval: DangerousToolAutoApproval::Disabled,
                    recorded_user_overrides: Box::new([]),
                    system_prompt: Some(prompt.clone()),
                    tool_entries: Box::new([]),
                })]
                .into(),
                calls: 0,
            },
            UnusedFailure,
            UnusedAuthorization,
            UnusedObservation,
            ScriptedModelCallProvider::new([ScriptedModelCallStep::CapabilityCancelled]),
            InProcessAttemptDispatchGate::default(),
            None,
        );
        assert_eq!(
            service
                .execute(session)
                .await
                .expect("durable cancellation is authoritative"),
            ModelCallExecutionOutcome::NoWork
        );
        let (_, _, _, _, _, provider, ..) = service.into_parts();
        assert_eq!(
            provider.last_prepared_system_prompt(),
            Some(Some(prompt.as_str()))
        );

        let mut promptless_service = ModelCallExecutionService::new(
            FixedIds::baseline(),
            FakePrepare {
                outcomes: [Ok(ready(request))].into(),
                calls: 0,
            },
            UnusedFailure,
            UnusedAuthorization,
            UnusedObservation,
            ScriptedModelCallProvider::new([ScriptedModelCallStep::CapabilityCancelled]),
            InProcessAttemptDispatchGate::default(),
            None,
        );
        assert_eq!(
            promptless_service
                .execute(session)
                .await
                .expect("durable cancellation is authoritative"),
            ModelCallExecutionOutcome::NoWork
        );
        let (_, _, _, _, _, provider, ..) = promptless_service.into_parts();
        assert_eq!(provider.last_prepared_system_prompt(), Some(None));
    }

    /// docs/spec/model-call-execution.md: a trustworthy capability failure
    /// survives a failed guarded closure and explicit service decomposition,
    /// then resubmits without repeating capability preparation.
    #[tokio::test]
    async fn capability_failure_commit_retains_evidence_across_handoff() {
        let (request, _) = prepared_fixture();
        let session = request.session();
        let turn = request.turn();
        let call = request.call().id();
        let mut service = ModelCallExecutionService::new(
            FixedIds::baseline(),
            FakePrepare {
                outcomes: [Ok(ready(request))].into(),
                calls: 0,
            },
            FakeFailure {
                errors: [FakeError::Infrastructure, FakeError::Infrastructure].into(),
                rereads: [Ok(RetainedPreparedFailureStatus::Pending)].into(),
                calls: 0,
                reread_calls: 0,
            },
            UnusedAuthorization,
            UnusedObservation,
            ScriptedModelCallProvider::new([ScriptedModelCallStep::CapabilityKnownFailure]),
            InProcessAttemptDispatchGate::default(),
            None,
        );

        assert!(matches!(
            service.execute(session).await,
            Err(ModelCallExecutionError::PreparedFailureCommit(
                FakeError::Infrastructure
            ))
        ));
        assert_eq!(
            service.retained_state(),
            Some(&RetainedModelCallExecutionState {
                state: RetainedModelCallExecutionStateKind::PreparedFailure {
                    session,
                    turn,
                    call,
                    cause: PreparedModelCallFailureCause::CapabilityKnownFailure,
                    attachment_failure: None,
                },
            })
        );

        let (
            ids,
            prepare,
            failure,
            authorization,
            observation,
            provider,
            gate,
            catalog,
            retained,
            tool_round_limit,
        ) = service.into_parts();
        assert_eq!(prepare.calls, 1);
        assert_eq!(failure.calls, 1);
        assert_eq!(failure.reread_calls, 0);
        assert_eq!(provider.capability_preparation_count(), 1);
        let mut resumed = ModelCallExecutionService::from_parts(
            ids,
            prepare,
            failure,
            authorization,
            observation,
            provider,
            gate,
            catalog,
            retained,
            tool_round_limit,
        );
        assert!(matches!(
            resumed.execute(identity(99, SessionId::from_uuid)).await,
            Err(ModelCallExecutionError::PreparedFailureCommit(
                FakeError::Infrastructure
            ))
        ));
        let (_, prepare, failure, _, _, provider, _, _, retained, _) = resumed.into_parts();
        assert_eq!(prepare.calls, 1);
        assert_eq!(failure.calls, 2);
        assert_eq!(failure.reread_calls, 1);
        assert_eq!(provider.capability_preparation_count(), 1);
        assert_eq!(
            retained,
            Some(RetainedModelCallExecutionState {
                state: RetainedModelCallExecutionStateKind::PreparedFailure {
                    session,
                    turn,
                    call,
                    cause: PreparedModelCallFailureCause::CapabilityKnownFailure,
                    attachment_failure: None,
                },
            })
        );
    }

    /// A capability failure that commits terminalizes the turn: the service
    /// returns the exact failed turn the transaction recorded and retains
    /// nothing to resubmit. Without this the turn stops with no terminal
    /// outcome recorded and stays non-terminal forever.
    #[tokio::test]
    async fn committed_capability_failure_returns_the_recorded_terminal_turn() {
        let (request, _) = prepared_fixture();
        let session = request.session();
        // Captured before the request moves into the fake. `ScriptedFailure`
        // returns the scripted failed turn whatever call it is handed, so a
        // commit addressed to a stale or unrelated call of the same session
        // would satisfy both the outcome comparison and a session-only
        // assertion while terminalizing the wrong call.
        let prepared_call = request.call().id();
        let failed = failed_turn_fixture();
        let mut service = ModelCallExecutionService::new(
            FixedIds::baseline(),
            FakePrepare {
                outcomes: [Ok(ready(request))].into(),
                calls: 0,
            },
            ScriptedFailure {
                results: [Ok(failed.clone())].into(),
                calls: 0,
                recorded: Vec::new(),
            },
            UnusedAuthorization,
            UnusedObservation,
            ScriptedModelCallProvider::new([ScriptedModelCallStep::CapabilityKnownFailure]),
            InProcessAttemptDispatchGate::default(),
            None,
        );

        assert_eq!(
            service
                .execute(session)
                .await
                .expect("the capability failure commits"),
            ModelCallExecutionOutcome::CapabilityKnownFailure(Box::new(failed))
        );
        let (_, prepare, failure, _, _, provider, _, _, retained, _) = service.into_parts();
        assert_eq!(prepare.calls, 1);
        assert_eq!(failure.calls, 1);
        // The failure must belong to the prepared turn's own session *and*
        // name the call that was prepared. Counting the attempt alone would
        // accept a terminal turn written against an unrelated fixture session,
        // and checking the session alone would accept one written against
        // another call of this session.
        assert_eq!(failure.recorded.len(), 1);
        let committed = &failure.recorded[0];
        assert_eq!(committed.session, session);
        assert_eq!(
            committed.call, prepared_call,
            "the committed failure must terminalize the prepared call"
        );
        assert_eq!(provider.interaction_count(), 0);
        assert!(retained.is_none());
    }

    /// a typed attachment failure terminalizes the prepared call
    /// before durable send authorization or provider interaction, and the
    /// exact attachment evidence reaches the guarded failure transaction.
    #[tokio::test]
    async fn attachment_failure_closes_before_durable_authorization() {
        let (request, _) = prepared_fixture();
        let session = request.session();
        let failed = failed_turn_fixture();
        let attachment_failure = AttachmentPreparationFailure::Missing;
        let mut service = ModelCallExecutionService::new(
            FixedIds::baseline(),
            FakePrepare {
                outcomes: [Ok(ready(request))].into(),
                calls: 0,
            },
            ScriptedFailure {
                results: [Ok(failed.clone())].into(),
                calls: 0,
                recorded: Vec::new(),
            },
            UnusedAuthorization,
            UnusedObservation,
            AttachmentFailureProvider {
                failure: attachment_failure,
                preparation_count: 0,
            },
            InProcessAttemptDispatchGate::default(),
            None,
        );

        assert_eq!(
            service
                .execute(session)
                .await
                .expect("the typed attachment failure commits before authorization"),
            ModelCallExecutionOutcome::CapabilityKnownFailure(Box::new(failed))
        );
        let (_, _, failure, _, _, provider, _, _, retained, _) = service.into_parts();
        assert_eq!(failure.calls, 1);
        let committed = &failure.recorded[0];
        assert_eq!(
            committed.cause,
            PreparedModelCallFailureCause::CapabilityKnownFailure
        );
        assert_eq!(committed.attachment_failure, Some(attachment_failure));
        assert_eq!(provider.preparation_count, 1);
        assert!(retained.is_none());
    }

    /// unavailable attachment verification leaves the exact call
    /// prepared without durable failure or send authorization.
    #[tokio::test]
    async fn attachment_unavailable_leaves_prepared_without_authorization() {
        let (request, _) = prepared_fixture();
        let session = request.session();
        let mut service = ModelCallExecutionService::new(
            FixedIds::baseline(),
            FakePrepare {
                outcomes: [Ok(ready(request))].into(),
                calls: 0,
            },
            ScriptedFailure {
                results: [].into(),
                calls: 0,
                recorded: Vec::new(),
            },
            UnusedAuthorization,
            UnusedObservation,
            AttachmentFailureProvider {
                failure: AttachmentPreparationFailure::Unavailable,
                preparation_count: 0,
            },
            InProcessAttemptDispatchGate::default(),
            None,
        );

        assert_eq!(
            service
                .execute(session)
                .await
                .expect("unavailable attachment verification is a typed retryable outcome"),
            ModelCallExecutionOutcome::AttachmentUnavailable
        );
        let (_, _, failure, _, _, _, _, _, retained, _) = service.into_parts();
        assert_eq!(failure.calls, 0);
        assert!(retained.is_none());
    }

    /// An identity collision inside failure closure is retried with fresh
    /// identities instead of surfacing as an operator failure, so a raced
    /// terminalization still records exactly one terminal turn.
    #[tokio::test]
    async fn capability_failure_commit_retries_an_identity_collision() {
        let (request, _) = prepared_fixture();
        let session = request.session();
        // Captured before the request moves into the fake, so the retry is
        // compared against the call that was actually prepared rather than only
        // against itself.
        let prepared_call = request.call().id();
        let failed = failed_turn_fixture();
        let mut service = ModelCallExecutionService::new(
            FixedIds::baseline(),
            FakePrepare {
                outcomes: [Ok(ready(request))].into(),
                calls: 0,
            },
            ScriptedFailure {
                results: [Err(FakeError::IdentityCollision), Ok(failed.clone())].into(),
                calls: 0,
                recorded: Vec::new(),
            },
            UnusedAuthorization,
            UnusedObservation,
            ScriptedModelCallProvider::new([ScriptedModelCallStep::CapabilityKnownFailure]),
            InProcessAttemptDispatchGate::default(),
            None,
        );

        assert_eq!(
            service
                .execute(session)
                .await
                .expect("the retried capability failure commits"),
            ModelCallExecutionOutcome::CapabilityKnownFailure(Box::new(failed))
        );
        let (_, _, failure, _, _, provider, _, _, retained, _) = service.into_parts();
        assert_eq!(failure.calls, 2);
        // Both attempts must address the prepared call — agreeing only with
        // each other would still pass if the service handed the same stale or
        // unrelated call to both — and the second must carry *fresh*
        // identities, which is the whole point of retrying a collision. A
        // retry reusing the colliding identities would collide again forever.
        let [first, second] = failure.recorded.as_slice() else {
            panic!(
                "expected exactly two failure-commit attempts, got {}",
                failure.recorded.len()
            )
        };
        assert_eq!(first.session, session);
        assert_eq!(second.session, session);
        assert_eq!(first.call, prepared_call);
        assert_eq!(second.call, prepared_call);
        // Every component of the bundle has to be refreshed, not just one.
        // Whole-bundle inequality passes when a retry mints a new failure
        // entry but reuses the terminal frontier (or the reverse), and if the
        // reused component is the one that collided the real transaction
        // rejects every retry and the turn stays wedged forever.
        assert_ne!(
            first.identities.failure_entry(),
            second.identities.failure_entry(),
            "a retried identity collision must mint a fresh failure entry"
        );
        assert_ne!(
            first.identities.terminal_frontier(),
            second.identities.terminal_frontier(),
            "a retried identity collision must mint a fresh terminal frontier"
        );
        assert_eq!(provider.capability_preparation_count(), 1);
        assert!(retained.is_none());
    }

    /// S15: a turn that reaches the automatic tool-round limit closes
    /// with its distinct terminal reason before provider entry. This prevents
    /// a runaway paid provider loop without misreporting saturation as a
    /// capability failure.
    #[tokio::test]
    async fn s15_tool_round_limit_fires_before_provider_entry() {
        const CONFIGURED_TOOL_ROUND_LIMIT: usize = 7;
        let (request, tool_entries, failed) =
            tool_round_saturated_fixture(CONFIGURED_TOOL_ROUND_LIMIT);
        let session = request.session();
        // Captured before the request moves into the fake: the committed
        // terminalization has to name *this* saturated call.
        let saturated_call = request.call().id();
        let mut service = ModelCallExecutionService::new(
            FixedIds::baseline(),
            FakePrepare {
                outcomes: [Ok(ready_with_tool_evidence(request, tool_entries))].into(),
                calls: 0,
            },
            ScriptedFailure {
                results: [Ok(failed.clone())].into(),
                calls: 0,
                recorded: Vec::new(),
            },
            UnusedAuthorization,
            UnusedObservation,
            ScriptedModelCallProvider::new([]),
            InProcessAttemptDispatchGate::default(),
            Some(CONFIGURED_TOOL_ROUND_LIMIT),
        );
        let captured = CapturedTelemetry::default();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(captured.clone())
            .finish();

        assert_eq!(
            service
                .execute(session)
                .with_subscriber(subscriber)
                .await
                .expect("the saturated turn closes with its own terminal reason"),
            ModelCallExecutionOutcome::ToolRoundLimitReached(Box::new(failed))
        );
        assert!(
            captured
                .text()
                .contains("terminal_outcome=\"tool_round_limit_reached\""),
            "the service terminalization must expose the  label"
        );
        let (_, prepare, failure, _, _, provider, _, _, retained, _) = service.into_parts();
        assert_eq!(prepare.calls, 1);
        assert_eq!(failure.calls, 1);
        // A count alone accepts `fail_prepared` called with an unrelated
        // session or call, which would terminalize something other than the
        // saturated turn while this test still claimed it was closed.
        assert_eq!(failure.recorded.len(), 1);
        let committed = &failure.recorded[0];
        assert_eq!(committed.session, session);
        assert_eq!(committed.call, saturated_call);
        assert_eq!(
            committed.cause,
            PreparedModelCallFailureCause::ToolRoundLimitReached
        );
        assert_eq!(
            provider.capability_preparation_count(),
            0,
            "a saturated turn must not reach provider capability preparation"
        );
        assert_eq!(
            provider.interaction_count(),
            0,
            "a saturated turn must not reach provider interaction"
        );
        assert!(retained.is_none());
    }

    /// Sums the content the rendered messages hold, message kind by message
    /// kind.
    ///
    /// This is the renderer's side of the accounting-fidelity comparison. It
    /// reads what each message carries *after* the clone, so a term the ceiling
    /// forgot surfaces as a difference instead of as two copies of the same
    /// omission agreeing with each other.
    fn rendered_content_bytes(messages: &[ModelConversationMessage]) -> usize {
        messages.iter().fold(0_usize, |total, message| {
            let bytes = match message {
                ModelConversationMessage::ContextSummary { content, .. }
                | ModelConversationMessage::Assistant { content, .. } => content.as_str().len(),
                // Mirrors `user_content_text_bytes`: attachment stubs carry a
                // fixed-width digest and bounded declarations held under
                // `MAX_RENDERED_ATTACHMENT_STUB_BYTES`, so they sit outside the
                // retained-content sum on both sides of this comparison.
                ModelConversationMessage::User { content, .. } => {
                    content
                        .parts()
                        .iter()
                        .fold(0_usize, |total, part| match part {
                            ModelUserContentPart::Text(value) => {
                                total.saturating_add(value.as_str().len())
                            }
                            ModelUserContentPart::AttachmentStub(_) => total,
                        })
                }
                ModelConversationMessage::DelegatedTask { content, .. }
                | ModelConversationMessage::DelegationMessage { content, .. } => {
                    content.as_str().len()
                }
                ModelConversationMessage::BackgroundDelegationResult { outcome, .. } => outcome
                    .content()
                    .map_or(0, |content| content.as_str().len()),
                ModelConversationMessage::AssistantToolUse { request, .. } => {
                    request.arguments().as_str().len()
                }
                ModelConversationMessage::ToolResult { content, .. } => match content {
                    ModelToolResultContent::Success(ToolResultContent::Text(text)) => {
                        text.as_str().len()
                    }
                    ModelToolResultContent::ExecutionError(error) => {
                        error.detail().map_or(0, |detail| detail.as_str().len())
                    }
                    ModelToolResultContent::Denied { reason } => {
                        reason.as_ref().map_or(0, |reason| reason.as_str().len())
                    }
                    ModelToolResultContent::ClosedByTurnEnd => 0,
                    ModelToolResultContent::Delegation(outcome) => outcome
                        .content()
                        .map_or(0, |content| content.as_str().len()),
                },
                ModelConversationMessage::ImportedUser { content, .. }
                | ModelConversationMessage::ImportedAssistant { content, .. } => {
                    content.as_str().len()
                }
                // An identity change carries fixed-width facts only.
                ModelConversationMessage::ModelIdentityChanged { .. } => 0,
            };
            total + bytes
        })
    }

    /// Counts one prepared request's projected content the way the ceiling
    /// does, reading the durable frontier and its origin content by reference.
    fn counted_frontier_bytes(
        request: &PreparedModelCallRequest,
        tool_entries: &[ResolvedToolConversationEntry],
    ) -> usize {
        projected_frontier_content_bytes(
            request
                .frontier_entry_slice()
                .iter()
                .map(|entry| (entry.reference(), entry.payload())),
            |accepted_input| request.origin_content(accepted_input),
            tool_entries.iter(),
        )
    }

    /// The retained-content accounting counts exactly what a render clones,
    /// including the frontier content no tool evidence names. Byte accounting
    /// that drifts from the renderer would let the ceiling admit more content
    /// than it names, so the sum is checked against the messages the same
    /// frontier actually produces — with assistant text and origin user content
    /// present, which a tool-only accounting would clone without counting.
    #[test]
    fn projected_frontier_content_bytes_matches_the_render_over_tool_and_text_entries() {
        let assistant_text = "the assistant narrates the round it is about to run";
        let (request, tool_entries, _) =
            tool_round_saturated_fixture_with_assistant_text(3, Some(assistant_text));
        let counted = counted_frontier_bytes(&request, &tool_entries);
        let no_entries: [(SemanticTranscriptEntryRef, &SemanticTranscriptEntryPayload); 0] = [];
        let tool_evidence_only =
            projected_frontier_content_bytes(no_entries, |_| None, tool_entries.iter());
        let operation = PreparedModelOperation::render(
            request,
            credential_reference(),
            None,
            Box::new([]),
            &tool_entries,
        )
        .expect("the fixture frontier renders");
        let rendered = rendered_content_bytes(operation.messages());
        assert!(
            tool_evidence_only > 0,
            "the fixture must retain tool content for the comparison to mean anything"
        );
        assert_eq!(
            counted, rendered,
            "the ceiling must count exactly the bytes the renderer clones"
        );
        assert!(
            counted >= tool_evidence_only + assistant_text.len(),
            "the ceiling must count the frontier content no tool evidence names: \
             counted {counted}, tool evidence {tool_evidence_only}"
        );
    }

    /// Every payload kind the renderer clones is counted, term for term.
    ///
    /// The fixture frontier reaches only tool evidence, assistant text, and
    /// origin content. Delegation content, delivered outcomes, context
    /// summaries, and imported text are cloned by the same renderer and were
    /// the kinds a tool-only accounting left unbounded, so they are compared
    /// here against both the rendered messages and an explicit per-term sum.
    #[test]
    fn projected_frontier_content_bytes_counts_every_payload_kind_the_render_clones() {
        let session = identity(300, SessionId::from_uuid);
        let child = identity(301, SessionId::from_uuid);
        let turn = identity(302, TurnId::from_uuid);
        let child_turn = identity(303, TurnId::from_uuid);
        let producing_call = identity(304, ModelCallId::from_uuid);
        let spawning_request = identity(305, ToolRequestId::from_uuid);
        let awaiting_request = identity(306, ToolRequestId::from_uuid);
        let background_request = identity(307, ToolRequestId::from_uuid);
        let peer_message = identity(308, DelegationMessageId::from_uuid);
        let origin_input = identity(309, AcceptedInputId::from_uuid);
        let steering_input = identity(310, AcceptedInputId::from_uuid);
        let imported_entry = identity(311, ImportedTranscriptEntryId::from_uuid);
        let selected = identity(312, DirectModelSelection::from_uuid);
        let source = |value: u128| {
            SemanticTranscriptEntryRef::from_source(
                session,
                identity(value, SemanticTranscriptEntryId::from_uuid),
            )
        };

        let imported_user = ImportedText::new(String::from("imported user question"));
        let imported_assistant = ImportedText::new(String::from("imported assistant answer"));
        let unattested = ImportedText::new(String::from("source event type never rendered"));
        let origin_text = String::from("the origin request this turn answers");
        let steering_text = String::from("steering added mid-turn");
        let task = DelegationContent::try_new(String::from("the delegated task"))
            .expect("fixture task content is valid");
        let peer = DelegationContent::try_new(String::from("a peer message"))
            .expect("fixture peer content is valid");
        let foreground_result = DelegationContent::try_new(String::from("the awaited result"))
            .expect("fixture foreground content is valid");
        let background_result = DelegationContent::try_new(String::from("a later child result"))
            .expect("fixture background content is valid");
        let summary = AssistantText::try_new(String::from("a summary standing in for a range"))
            .expect("fixture summary text is valid");
        let assistant = AssistantText::try_new(String::from("assistant prose with no bound"))
            .expect("fixture assistant text is valid");
        let outcome = |content: DelegationContent| {
            DelegationOutcome::reconstitute(
                signalbox_domain::DelegationOutcomeKind::ResultReturned,
                Some(content),
                signalbox_domain::DelegationOutcomeReason::ChildCompleted,
                signalbox_domain::DelegationProvenanceReconstitutionInput::ChildTurn {
                    session: child,
                    turn: child_turn,
                },
            )
            .expect("fixture child outcome is correlated")
        };
        let origin_contents = BTreeMap::from([
            (
                origin_input,
                UserContent::try_text(origin_text.clone()).expect("fixture origin text is valid"),
            ),
            (
                steering_input,
                UserContent::try_text(steering_text.clone())
                    .expect("fixture steering text is valid"),
            ),
        ]);

        let entries = [
            (
                source(400),
                SemanticTranscriptEntryPayload::Imported {
                    imported_entry,
                    source_speaker: ImportedSourceAttestation::Attested(ImportedSpeaker::User),
                    content: ImportedTranscriptContent::Text(ImportedSourceAttestation::Attested(
                        imported_user.clone(),
                    )),
                },
            ),
            (
                source(401),
                SemanticTranscriptEntryPayload::Imported {
                    imported_entry,
                    source_speaker: ImportedSourceAttestation::Attested(ImportedSpeaker::Assistant),
                    content: ImportedTranscriptContent::Text(ImportedSourceAttestation::Attested(
                        imported_assistant.clone(),
                    )),
                },
            ),
            // Renders no message at all, so it must contribute no bytes.
            (
                source(402),
                SemanticTranscriptEntryPayload::Imported {
                    imported_entry,
                    source_speaker: ImportedSourceAttestation::NotAttested,
                    content: ImportedTranscriptContent::SourceEvent {
                        source_type: ImportedSourceAttestation::Attested(unattested),
                    },
                },
            ),
            (
                source(403),
                SemanticTranscriptEntryPayload::OriginAcceptedInput {
                    accepted_input: origin_input,
                },
            ),
            (
                source(404),
                SemanticTranscriptEntryPayload::SteeringAcceptedInput {
                    accepted_input: steering_input,
                    source_turn: turn,
                },
            ),
            (
                source(405),
                SemanticTranscriptEntryPayload::DelegatedTask {
                    spawning_request,
                    parent_session: session,
                    parent_turn: turn,
                    content: task.clone(),
                },
            ),
            (
                source(406),
                SemanticTranscriptEntryPayload::DelegationMessage {
                    spawning_request,
                    message: peer_message,
                    sender: session,
                    recipient: child,
                    delivery_sequence: NonZeroU64::MIN,
                    content: peer.clone(),
                },
            ),
            (
                source(407),
                SemanticTranscriptEntryPayload::DelegationResult {
                    awaiting_request,
                    spawning_request,
                    child,
                    mode: DelegationWaitMode::Foreground,
                    delivery_sequence: None,
                    outcome: Box::new(outcome(foreground_result.clone())),
                },
            ),
            (
                source(408),
                SemanticTranscriptEntryPayload::DelegationResult {
                    awaiting_request: background_request,
                    spawning_request,
                    child,
                    mode: DelegationWaitMode::Background,
                    delivery_sequence: Some(NonZeroU64::new(2).expect("two is positive")),
                    outcome: Box::new(outcome(background_result.clone())),
                },
            ),
            (
                source(409),
                SemanticTranscriptEntryPayload::ContextSummary {
                    producing_call,
                    summarized: ContextCompactionRange::inclusive(source(400), source(401)),
                    value: summary.clone(),
                },
            ),
            (
                source(410),
                SemanticTranscriptEntryPayload::AssistantText {
                    producing_call,
                    value: assistant.clone(),
                },
            ),
            (
                source(411),
                SemanticTranscriptEntryPayload::ModelIdentityChanged {
                    turn,
                    defaults_version: SessionConfigurationDefaultsVersion::first(),
                    selected,
                },
            ),
            (
                source(412),
                SemanticTranscriptEntryPayload::TurnCompleted { turn },
            ),
        ];

        let counted = projected_frontier_content_bytes(
            entries.iter().map(|(source, payload)| (*source, payload)),
            |accepted_input| origin_contents.get(&accepted_input),
            std::iter::empty(),
        );
        let messages = render_frontier_messages(
            entries.iter().map(|(source, payload)| (*source, payload)),
            |accepted_input| origin_contents.get(&accepted_input).cloned(),
            |_| None,
            std::iter::empty(),
        )
        .expect("the payload fixture renders");

        assert_eq!(
            counted,
            rendered_content_bytes(&messages),
            "the ceiling must count exactly the bytes the renderer clones"
        );
        // An equality between two sums can also be satisfied by both sides
        // dropping the same term, so the expected total is spelled out.
        assert_eq!(
            counted,
            imported_user.as_str().len()
                + imported_assistant.as_str().len()
                + origin_text.len()
                + steering_text.len()
                + task.as_str().len()
                + peer.as_str().len()
                + foreground_result.as_str().len()
                + background_result.as_str().len()
                + summary.as_str().len()
                + assistant.as_str().len(),
            "every cloned payload kind must contribute its exact content bytes"
        );
    }

    /// A frontier over-bound only by its assistant text is refused, and refused
    /// before anything is cloned.
    ///
    /// Assistant text carries no length bound of its own beyond the transport
    /// cap on a single response, so a ceiling that counted tool evidence alone
    /// would clone this frontier while reporting it as within bounds. The limit
    /// here is exactly the same frontier's byte count without the text, which
    /// makes the text the only reason the render is refused; the unrenderable
    /// variant then shows the refusal still precedes message construction.
    #[test]
    fn assistant_text_over_bound_frontiers_are_refused_before_any_clone() {
        let assistant_text = "assistant prose that no tool-evidence accounting would ever see";
        let (plain_request, plain_entries, _) = tool_round_saturated_fixture(2);
        let plain_bytes = counted_frontier_bytes(&plain_request, &plain_entries);
        // Control: at this exact ceiling the same frontier without the text
        // renders, so the refusal below is caused by the text and not by a
        // ceiling too small for the fixture's tool evidence.
        PreparedModelOperation::render_within(
            plain_request,
            credential_reference(),
            None,
            Box::new([]),
            &plain_entries,
            plain_bytes,
        )
        .expect("the text-free frontier renders at its own byte count");

        let (request, tool_entries, _) =
            tool_round_saturated_fixture_with_assistant_text(2, Some(assistant_text));
        let error = PreparedModelOperation::render_within(
            request.clone(),
            credential_reference(),
            None,
            Box::new([]),
            &tool_entries,
            plain_bytes,
        )
        .expect_err("an assistant-text-heavy frontier is refused");
        assert_eq!(
            error,
            ModelFrontierRenderingError::RetainedFrontierContentLimitExceeded {
                observed_bytes: plain_bytes + assistant_text.len(),
                limit_bytes: plain_bytes,
            },
            "the refusal must report the assistant text it counted"
        );

        // The same refusal still wins over a rendering failure, which places it
        // before the clones the renderer would perform.
        let mut unrenderable = tool_entries.into_vec();
        unrenderable.push(
            unrenderable
                .first()
                .expect("the fixture carries tool evidence")
                .clone(),
        );
        assert!(
            matches!(
                PreparedModelOperation::render_within(
                    request.clone(),
                    credential_reference(),
                    None,
                    Box::new([]),
                    &unrenderable,
                    MAX_RETAINED_FRONTIER_CONTENT_BYTES,
                ),
                Err(ModelFrontierRenderingError::DuplicateToolEvidence { .. })
            ),
            "the duplicated evidence must be unrenderable for this ordering claim to hold"
        );
        assert!(
            matches!(
                PreparedModelOperation::render_within(
                    request,
                    credential_reference(),
                    None,
                    Box::new([]),
                    &unrenderable,
                    plain_bytes,
                ),
                Err(ModelFrontierRenderingError::RetainedFrontierContentLimitExceeded { .. })
            ),
            "the content ceiling must win over the rendering failure it precedes"
        );
    }

    /// The ceiling is enforced ahead of message construction. A frontier that is
    /// both over-bound and unrenderable is refused for its content, which places
    /// the guard before `render_frontier_messages` and therefore before the
    /// clones that would exhaust memory — the ordering a guard reached only
    /// after rendering cannot provide.
    #[test]
    fn retained_frontier_content_ceiling_precedes_message_rendering() {
        let (request, tool_entries, _) = tool_round_saturated_fixture(2);
        let mut unrenderable = tool_entries.into_vec();
        unrenderable.push(
            unrenderable
                .first()
                .expect("the fixture carries tool evidence")
                .clone(),
        );
        // Control: with the ceiling out of the way this evidence fails inside
        // the renderer, so the refusal below is genuinely the earlier one.
        assert!(
            matches!(
                PreparedModelOperation::render_within(
                    request.clone(),
                    credential_reference(),
                    None,
                    Box::new([]),
                    &unrenderable,
                    MAX_RETAINED_FRONTIER_CONTENT_BYTES,
                ),
                Err(ModelFrontierRenderingError::DuplicateToolEvidence { .. })
            ),
            "the fixture must be unrenderable for this ordering claim to hold"
        );
        let error = PreparedModelOperation::render_within(
            request,
            credential_reference(),
            None,
            Box::new([]),
            &unrenderable,
            0,
        )
        .expect_err("an over-bound frontier is refused");
        assert!(
            matches!(
                error,
                ModelFrontierRenderingError::RetainedFrontierContentLimitExceeded {
                    limit_bytes: 0,
                    ..
                }
            ),
            "the content ceiling must win over the rendering failure it precedes: {error:?}"
        );
    }

    /// S15: a turn whose retained tool content exceeds its ceiling
    /// closes through the same pre-send terminal contract as round saturation
    /// and never enters the provider. The round ceiling alone bounds latency and
    /// spend but not retained memory, which is what this bound supplies.
    #[tokio::test]
    async fn s15_retained_frontier_content_limit_fires_before_provider_entry() {
        // Two rounds and no configured round ceiling at all, so only the
        // retained-content bound can explain the closure.
        let (request, tool_entries, failed) = tool_round_saturated_fixture(2);
        let session = request.session();
        let over_bound_call = request.call().id();
        let mut service = ModelCallExecutionService::new(
            FixedIds::baseline(),
            FakePrepare {
                outcomes: [Ok(ready_with_tool_evidence(request, tool_entries))].into(),
                calls: 0,
            },
            ScriptedFailure {
                results: [Ok(failed.clone())].into(),
                calls: 0,
                recorded: Vec::new(),
            },
            UnusedAuthorization,
            UnusedObservation,
            ScriptedModelCallProvider::new([]),
            InProcessAttemptDispatchGate::default(),
            None,
        )
        .with_retained_frontier_content_limit(0);
        let captured = CapturedTelemetry::default();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(captured.clone())
            .finish();

        assert_eq!(
            service
                .execute(session)
                .with_subscriber(subscriber)
                .await
                .expect("the over-bound turn closes with its own terminal reason"),
            ModelCallExecutionOutcome::ToolRoundLimitReached(Box::new(failed))
        );
        let telemetry = captured.text();
        assert!(
            telemetry.contains("retained frontier content limit reached"),
            "the refusal must name the bound that fired: {telemetry}"
        );
        assert!(
            telemetry.contains("terminal_outcome=\"tool_round_limit_reached\""),
            "the service terminalization must expose the  label"
        );
        let (_, prepare, failure, _, _, provider, _, _, retained, _) = service.into_parts();
        assert_eq!(prepare.calls, 1);
        assert_eq!(failure.calls, 1);
        assert_eq!(failure.recorded.len(), 1);
        let committed = &failure.recorded[0];
        assert_eq!(committed.session, session);
        assert_eq!(committed.call, over_bound_call);
        assert_eq!(
            committed.cause,
            PreparedModelCallFailureCause::ToolRoundLimitReached
        );
        assert_eq!(
            provider.capability_preparation_count(),
            0,
            "an over-bound turn must not reach provider capability preparation"
        );
        assert_eq!(
            provider.interaction_count(),
            0,
            "an over-bound turn must not reach provider interaction"
        );
        assert!(retained.is_none());
    }

    /// an ambiguous tool-round-limit closure retains its exact cause,
    /// then an authoritative reread maps the landed closure to the distinct
    /// already-committed outcome without entering the provider.
    #[tokio::test]
    async fn tool_round_limit_ambiguous_commit_round_trips_retained_cause() {
        const CONFIGURED_TOOL_ROUND_LIMIT: usize = 5;
        let (request, tool_entries, _) = tool_round_saturated_fixture(CONFIGURED_TOOL_ROUND_LIMIT);
        let session = request.session();
        let turn = request.turn();
        let call = request.call().id();
        let mut service = ModelCallExecutionService::new(
            FixedIds::baseline(),
            FakePrepare {
                outcomes: [Ok(ready_with_tool_evidence(request, tool_entries))].into(),
                calls: 0,
            },
            FakeFailure {
                errors: [FakeError::CommitAmbiguous].into(),
                rereads: [Ok(RetainedPreparedFailureStatus::AlreadyCommitted)].into(),
                calls: 0,
                reread_calls: 0,
            },
            UnusedAuthorization,
            UnusedObservation,
            ScriptedModelCallProvider::new([]),
            InProcessAttemptDispatchGate::default(),
            Some(CONFIGURED_TOOL_ROUND_LIMIT),
        );

        assert!(matches!(
            service.execute(session).await,
            Err(ModelCallExecutionError::PreparedFailureCommit(
                FakeError::CommitAmbiguous
            ))
        ));
        assert_eq!(
            service.retained_state(),
            Some(&RetainedModelCallExecutionState {
                state: RetainedModelCallExecutionStateKind::PreparedFailure {
                    session,
                    turn,
                    call,
                    cause: PreparedModelCallFailureCause::ToolRoundLimitReached,
                    attachment_failure: None,
                },
            })
        );
        assert_eq!(
            service
                .execute(identity(99, SessionId::from_uuid))
                .await
                .expect("the reread proves the tool-round closure landed"),
            ModelCallExecutionOutcome::ToolRoundLimitAlreadyCommitted(call)
        );
        let (_, prepare, failure, _, _, provider, _, _, retained, _) = service.into_parts();
        assert_eq!(prepare.calls, 1);
        assert_eq!(failure.calls, 1);
        assert_eq!(failure.reread_calls, 1);
        assert_eq!(provider.capability_preparation_count(), 0);
        assert_eq!(provider.interaction_count(), 0);
        assert!(retained.is_none());
    }

    /// if an interrupt wins after capability preparation reported a
    /// known failure, the retained reread accepts the durable cancellation as
    /// authoritative no-work rather than retrying failure closure forever.
    #[tokio::test]
    async fn capability_failure_race_rereads_cancellation_as_no_work() {
        let (request, _) = prepared_fixture();
        let session = request.session();
        let mut service = ModelCallExecutionService::new(
            FixedIds::baseline(),
            FakePrepare {
                outcomes: [Ok(ready(request))].into(),
                calls: 0,
            },
            FakeFailure {
                errors: [FakeError::Infrastructure].into(),
                rereads: [Ok(RetainedPreparedFailureStatus::Cancelled)].into(),
                calls: 0,
                reread_calls: 0,
            },
            UnusedAuthorization,
            UnusedObservation,
            ScriptedModelCallProvider::new([ScriptedModelCallStep::CapabilityKnownFailure]),
            InProcessAttemptDispatchGate::default(),
            None,
        );

        assert!(matches!(
            service.execute(session).await,
            Err(ModelCallExecutionError::PreparedFailureCommit(
                FakeError::Infrastructure
            ))
        ));
        assert_eq!(
            service
                .execute(identity(99, SessionId::from_uuid))
                .await
                .expect("the cancellation reread is authoritative"),
            ModelCallExecutionOutcome::NoWork
        );
        let (_, prepare, failure, _, _, provider, _, _, retained, _) = service.into_parts();
        assert_eq!(prepare.calls, 1);
        assert_eq!(failure.calls, 1);
        assert_eq!(failure.reread_calls, 1);
        assert_eq!(provider.capability_preparation_count(), 1);
        assert!(retained.is_none());
    }

    /// docs/spec/model-call-execution.md: a commit-ambiguous
    /// capability-failure closure is reread before any resubmission, and a
    /// landed closure ends reconciliation without repeating credential
    /// preparation or the guarded transaction.
    #[tokio::test]
    async fn ambiguous_capability_failure_commit_is_reread_before_resubmission() {
        let (request, _) = prepared_fixture();
        let session = request.session();
        let call = request.call().id();
        let mut service = ModelCallExecutionService::new(
            FixedIds::baseline(),
            FakePrepare {
                outcomes: [Ok(ready(request))].into(),
                calls: 0,
            },
            FakeFailure {
                errors: [FakeError::CommitAmbiguous].into(),
                rereads: [Ok(RetainedPreparedFailureStatus::AlreadyCommitted)].into(),
                calls: 0,
                reread_calls: 0,
            },
            UnusedAuthorization,
            UnusedObservation,
            ScriptedModelCallProvider::new([ScriptedModelCallStep::CapabilityKnownFailure]),
            InProcessAttemptDispatchGate::default(),
            None,
        );

        assert!(matches!(
            service.execute(session).await,
            Err(ModelCallExecutionError::PreparedFailureCommit(
                FakeError::CommitAmbiguous
            ))
        ));
        assert_eq!(
            service
                .execute(identity(99, SessionId::from_uuid))
                .await
                .expect("the authoritative reread proves the closure landed"),
            ModelCallExecutionOutcome::CapabilityFailureAlreadyCommitted(call)
        );
        let (_, prepare, failure, _, _, provider, _, _, retained, _) = service.into_parts();
        assert_eq!(prepare.calls, 1);
        assert_eq!(failure.calls, 1);
        assert_eq!(failure.reread_calls, 1);
        assert_eq!(provider.capability_preparation_count(), 1);
        assert!(retained.is_none());
    }

    /// docs/spec/model-call-execution.md: send authorization has no fresh
    /// candidate to replace after an identity-collision classification, so
    /// the same session/call pair is not retried in place.
    #[tokio::test]
    async fn authorization_identity_collision_returns_without_retrying_same_call() {
        let (request, _) = prepared_fixture();
        let session = request.session();
        let mut service = ModelCallExecutionService::new(
            FixedIds::baseline(),
            FakePrepare {
                outcomes: [Ok(ready(request))].into(),
                calls: 0,
            },
            UnusedFailure,
            FakeAuthorization {
                outcomes: [Err(FakeError::IdentityCollision)].into(),
                rereads: VecDeque::new(),
                calls: 0,
                reread_calls: 0,
            },
            UnusedObservation,
            ScriptedModelCallProvider::new([ScriptedModelCallStep::Return(
                ModelCallTerminalObservation::KnownFailed,
            )]),
            InProcessAttemptDispatchGate::default(),
            None,
        );

        assert!(matches!(
            service.execute(session).await,
            Err(ModelCallExecutionError::Authorization(
                FakeError::IdentityCollision
            ))
        ));
        let (_, prepare, _, authorization, _, provider, _, _, retained, _) = service.into_parts();
        assert_eq!(prepare.calls, 1);
        assert_eq!(authorization.calls, 1);
        assert_eq!(authorization.reread_calls, 0);
        assert_eq!(provider.capability_preparation_count(), 1);
        assert_eq!(provider.interaction_count(), 0);
        assert!(retained.is_none());
    }

    /// docs/spec/model-call-execution.md: stale or stopped authority is an
    /// ordinary no-send result, not a caller/hub defect and never provider
    /// entry.
    #[tokio::test]
    async fn stale_authorization_returns_no_work_without_provider_entry() {
        let (request, _) = prepared_fixture();
        let session = request.session();
        let mut service = ModelCallExecutionService::new(
            FixedIds::baseline(),
            FakePrepare {
                outcomes: [Ok(ready(request))].into(),
                calls: 0,
            },
            UnusedFailure,
            NoSendAuthorization { calls: 0 },
            UnusedObservation,
            ScriptedModelCallProvider::new([ScriptedModelCallStep::Return(
                ModelCallTerminalObservation::KnownFailed,
            )]),
            InProcessAttemptDispatchGate::default(),
            None,
        );

        assert_eq!(
            service
                .execute(session)
                .await
                .expect("stale authority is a normal no-send result"),
            ModelCallExecutionOutcome::NoWork
        );
        let (_, _, _, authorization, _, provider, _, _, retained, _) = service.into_parts();
        assert_eq!(authorization.calls, 1);
        assert_eq!(provider.capability_preparation_count(), 1);
        assert_eq!(provider.interaction_count(), 0);
        assert!(retained.is_none());
    }

    /// A resumed prepared call constructs one opaque
    /// capability, commits InFlight first, and invokes the provider once. An
    /// operator failure produces no fabricated observation commit.
    #[tokio::test]
    async fn resumed_provider_failure_stays_at_provider_stage() {
        let (request, authorized) = prepared_fixture();
        let mut service = ModelCallExecutionService::new(
            FixedIds::baseline(),
            FakePrepare {
                outcomes: [Ok(ready(request))].into(),
                calls: 0,
            },
            UnusedFailure,
            FakeAuthorization {
                outcomes: [Ok(authorized)].into(),
                rereads: VecDeque::new(),
                calls: 0,
                reread_calls: 0,
            },
            UnusedObservation,
            ScriptedModelCallProvider::new([ScriptedModelCallStep::InteractionOperatorFailure]),
            InProcessAttemptDispatchGate::default(),
            None,
        );
        let error = service
            .execute(identity(1, SessionId::from_uuid))
            .await
            .expect_err("the script reports no trustworthy observation");
        assert!(matches!(
            error,
            ModelCallExecutionError::Provider(ScriptedModelCallError::InteractionOperatorFailure)
        ));
        let (_, prepare, _, authorization, _, provider, _, _, _, _) = service.into_parts();
        assert_eq!(prepare.calls, 1);
        assert_eq!(authorization.calls, 1);
        assert_eq!(provider.capability_preparation_count(), 1);
        assert_eq!(provider.interaction_count(), 1);
    }

    /// S02: a non-collision observation failure retains
    /// the exact result; later passes authoritatively resubmit it unchanged
    /// while absent and stop once the original commit is observed.
    #[tokio::test]
    async fn s02_failed_observation_commit_is_retained_and_reread() {
        let (request, authorized) = prepared_fixture();
        let call = authorized.call().id();
        let session = authorized.session();
        let mut service = ModelCallExecutionService::new(
            FixedIds::baseline(),
            FakePrepare {
                outcomes: [Ok(ready(request))].into(),
                calls: 0,
            },
            UnusedFailure,
            FakeAuthorization {
                outcomes: [Ok(authorized)].into(),
                rereads: VecDeque::new(),
                calls: 0,
                reread_calls: 0,
            },
            FakeObservation {
                commit_errors: [FakeError::Infrastructure, FakeError::Infrastructure].into(),
                rereads: [
                    Ok(RetainedModelCallObservationStatus::Pending),
                    Ok(RetainedModelCallObservationStatus::AlreadyCommitted),
                ]
                .into(),
                observed: Vec::new(),
                commit_calls: 0,
                reread_calls: 0,
            },
            ScriptedModelCallProvider::new([ScriptedModelCallStep::Return(
                ModelCallTerminalObservation::KnownFailed,
            )]),
            InProcessAttemptDispatchGate::default(),
            None,
        );

        let error = service
            .execute(session)
            .await
            .expect_err("the first observation commit fails");
        let retained = match error {
            ModelCallExecutionError::ObservationCommit {
                error: FakeError::Infrastructure,
                retained_observation,
            } => retained_observation,
            error => panic!("unexpected failure: {error}"),
        };
        assert_eq!(retained.call(), call);
        assert_eq!(service.retained_observation(), Some(&retained));

        let error = service
            .execute(session)
            .await
            .expect_err("the unchanged resubmission also fails");
        let resubmitted = match error {
            ModelCallExecutionError::ObservationCommit {
                error: FakeError::Infrastructure,
                retained_observation,
            } => retained_observation,
            error => panic!("unexpected failure: {error}"),
        };
        assert_eq!(resubmitted, retained);

        assert_eq!(
            service
                .execute(session)
                .await
                .expect("the authoritative reread proves the retained commit landed"),
            ModelCallExecutionOutcome::ObservationAlreadyCommitted(call)
        );
        assert!(service.retained_observation().is_none());
        let (_, _, _, authorization, observation, provider, _, _, _, _) = service.into_parts();
        assert_eq!(authorization.calls, 1);
        assert_eq!(authorization.reread_calls, 0);
        assert_eq!(observation.commit_calls, 2);
        assert_eq!(observation.reread_calls, 2);
        assert_eq!(observation.observed, vec![retained.clone(), retained]);
        assert_eq!(provider.capability_preparation_count(), 1);
        assert_eq!(provider.interaction_count(), 1);
    }

    /// S02: when authorization acknowledgement is lost,
    /// the still-owned capability proves `invoke` was never entered. An
    /// authoritative InFlight reread becomes a correlated known-failure
    /// observation without any provider interaction.
    #[tokio::test]
    async fn s02_ambiguous_authorization_classifies_unconsumed_in_flight() {
        let (request, authorized) = prepared_fixture();
        let call = authorized.call().id();
        let session = authorized.session();
        let mut service = ModelCallExecutionService::new(
            FixedIds::baseline(),
            FakePrepare {
                outcomes: [Ok(ready(request))].into(),
                calls: 0,
            },
            UnusedFailure,
            FakeAuthorization {
                outcomes: [Err(FakeError::CommitAmbiguous)].into(),
                rereads: [Ok(ModelCallAuthorizationReread::InFlight(Box::new(
                    authorized,
                )))]
                .into(),
                calls: 0,
                reread_calls: 0,
            },
            FakeObservation {
                commit_errors: [FakeError::Infrastructure].into(),
                rereads: VecDeque::new(),
                observed: Vec::new(),
                commit_calls: 0,
                reread_calls: 0,
            },
            ScriptedModelCallProvider::new([ScriptedModelCallStep::Return(
                ModelCallTerminalObservation::Completed {
                    assistant_text: vec![
                        AssistantText::try_new(String::from("must not be sent"))
                            .expect("fixture text is valid"),
                    ],
                },
            )]),
            InProcessAttemptDispatchGate::default(),
            None,
        );

        let error = service
            .execute(session)
            .await
            .expect_err("the fake non-consumption commit fails visibly");
        let retained = match error {
            ModelCallExecutionError::ObservationCommit {
                error: FakeError::Infrastructure,
                retained_observation,
            } => retained_observation,
            error => panic!("unexpected failure: {error}"),
        };
        assert_eq!(retained.call(), call);
        assert_eq!(
            retained.observation(),
            &ModelCallTerminalObservation::KnownFailed
        );
        let (_, _, _, authorization, observation, provider, _, _, _, _) = service.into_parts();
        assert_eq!(authorization.calls, 1);
        assert_eq!(authorization.reread_calls, 1);
        assert_eq!(observation.commit_calls, 1);
        assert_eq!(observation.observed, vec![retained]);
        assert_eq!(provider.capability_preparation_count(), 1);
        assert_eq!(provider.interaction_count(), 0);
    }

    /// an ambiguous authorization reread accepts a complete
    /// concurrent direct cancellation of the exact unsent call as
    /// authoritative no-work without entering the provider.
    #[tokio::test]
    async fn ambiguous_authorization_accepts_terminal_cancellation() {
        let (request, _) = prepared_fixture();
        let session = request.session();
        let mut service = ModelCallExecutionService::new(
            FixedIds::baseline(),
            FakePrepare {
                outcomes: [Ok(ready(request))].into(),
                calls: 0,
            },
            UnusedFailure,
            FakeAuthorization {
                outcomes: [Err(FakeError::CommitAmbiguous)].into(),
                rereads: [Ok(ModelCallAuthorizationReread::Cancelled)].into(),
                calls: 0,
                reread_calls: 0,
            },
            FakeObservation {
                commit_errors: VecDeque::new(),
                rereads: VecDeque::new(),
                observed: Vec::new(),
                commit_calls: 0,
                reread_calls: 0,
            },
            ScriptedModelCallProvider::new([ScriptedModelCallStep::Return(
                ModelCallTerminalObservation::KnownFailed,
            )]),
            InProcessAttemptDispatchGate::default(),
            None,
        );

        assert_eq!(
            service
                .execute(session)
                .await
                .expect("the complete terminal cancellation is authoritative"),
            ModelCallExecutionOutcome::NoWork
        );
        let (_, _, _, authorization, observation, provider, _, _, retained, _) =
            service.into_parts();
        assert_eq!(authorization.calls, 1);
        assert_eq!(authorization.reread_calls, 1);
        assert_eq!(observation.commit_calls, 0);
        assert_eq!(provider.capability_preparation_count(), 1);
        assert_eq!(provider.interaction_count(), 0);
        assert!(retained.is_none());
    }

    /// docs/spec/model-call-execution.md: a failed ambiguous-authorization
    /// reread retains the exact non-consumption proof across handoff and
    /// later classifies a committed `InFlight` authorization without invoking
    /// the provider.
    #[tokio::test]
    async fn ambiguous_authorization_reread_retains_non_consumption_across_handoff() {
        let (request, authorized) = prepared_fixture();
        let session = request.session();
        let call = request.call().id();
        let mut service = ModelCallExecutionService::new(
            FixedIds::baseline(),
            FakePrepare {
                outcomes: [Ok(ready(request.clone()))].into(),
                calls: 0,
            },
            UnusedFailure,
            FakeAuthorization {
                outcomes: [Err(FakeError::CommitAmbiguous)].into(),
                rereads: [
                    Err(FakeError::Infrastructure),
                    Ok(ModelCallAuthorizationReread::InFlight(Box::new(authorized))),
                ]
                .into(),
                calls: 0,
                reread_calls: 0,
            },
            FakeObservation {
                commit_errors: [FakeError::Infrastructure].into(),
                rereads: VecDeque::new(),
                observed: Vec::new(),
                commit_calls: 0,
                reread_calls: 0,
            },
            ScriptedModelCallProvider::new([ScriptedModelCallStep::Return(
                ModelCallTerminalObservation::Completed {
                    assistant_text: vec![
                        AssistantText::try_new(String::from("must not be sent"))
                            .expect("fixture text is valid"),
                    ],
                },
            )]),
            InProcessAttemptDispatchGate::default(),
            None,
        );

        assert!(matches!(
            service.execute(session).await,
            Err(ModelCallExecutionError::AuthorizationReread {
                authorization_error: FakeError::CommitAmbiguous,
                reread_error: FakeError::Infrastructure,
            })
        ));
        assert!(matches!(
            service.retained_state(),
            Some(RetainedModelCallExecutionState {
                state: RetainedModelCallExecutionStateKind::AuthorizationNonConsumption {
                    session: retained_session,
                    prepared,
                },
            }) if *retained_session == session && **prepared == request
        ));

        let (
            ids,
            prepare,
            failure,
            authorization,
            observation,
            provider,
            gate,
            catalog,
            retained,
            tool_round_limit,
        ) = service.into_parts();
        let mut resumed = ModelCallExecutionService::from_parts(
            ids,
            prepare,
            failure,
            authorization,
            observation,
            provider,
            gate,
            catalog,
            retained,
            tool_round_limit,
        );
        let error = resumed
            .execute(identity(99, SessionId::from_uuid))
            .await
            .expect_err("the retained known-failure observation commit is visible");
        let retained_observation = match error {
            ModelCallExecutionError::ObservationCommit {
                error: FakeError::Infrastructure,
                retained_observation,
            } => retained_observation,
            error => panic!("unexpected reconciliation error: {error}"),
        };
        assert_eq!(retained_observation.call(), call);
        assert_eq!(
            retained_observation.observation(),
            &ModelCallTerminalObservation::KnownFailed
        );
        let (_, prepare, _, authorization, observation, provider, _, _, retained, _) =
            resumed.into_parts();
        assert_eq!(prepare.calls, 1);
        assert_eq!(authorization.calls, 1);
        assert_eq!(authorization.reread_calls, 2);
        assert_eq!(observation.commit_calls, 1);
        assert_eq!(provider.capability_preparation_count(), 1);
        assert_eq!(provider.interaction_count(), 0);
        assert!(matches!(
            retained,
            Some(RetainedModelCallExecutionState {
                state: RetainedModelCallExecutionStateKind::TerminalObservation {
                    observation,
                    ..
                },
            }) if observation.as_ref() == &retained_observation
        ));
    }

    /// when an ambiguous authorization is proven to have rolled
    /// back to Prepared, the unconsumed scripted interaction action can
    /// prepare again and still produces exactly one physical interaction.
    #[tokio::test]
    async fn s02_authorization_rollback_reprepares_one_scripted_interaction_action() {
        let (request, authorized) = prepared_fixture();
        let session = request.session();
        let mut service = ModelCallExecutionService::new(
            FixedIds::baseline(),
            FakePrepare {
                outcomes: [Ok(ready(request.clone())), Ok(ready(request))].into(),
                calls: 0,
            },
            UnusedFailure,
            FakeAuthorization {
                outcomes: [Err(FakeError::CommitAmbiguous), Ok(authorized)].into(),
                rereads: [
                    Err(FakeError::Infrastructure),
                    Ok(ModelCallAuthorizationReread::Prepared),
                ]
                .into(),
                calls: 0,
                reread_calls: 0,
            },
            FakeObservation {
                commit_errors: [FakeError::Infrastructure].into(),
                rereads: VecDeque::new(),
                observed: Vec::new(),
                commit_calls: 0,
                reread_calls: 0,
            },
            ScriptedModelCallProvider::new([ScriptedModelCallStep::Return(
                ModelCallTerminalObservation::KnownFailed,
            )]),
            InProcessAttemptDispatchGate::default(),
            None,
        );

        assert!(matches!(
            service.execute(session).await,
            Err(ModelCallExecutionError::AuthorizationReread {
                authorization_error: FakeError::CommitAmbiguous,
                reread_error: FakeError::Infrastructure,
            })
        ));
        assert!(matches!(
            service.execute(session).await,
            Err(ModelCallExecutionError::ObservationCommit {
                error: FakeError::Infrastructure,
                ..
            })
        ));

        let (_, prepare, _, authorization, observation, provider, _, _, _, _) =
            service.into_parts();
        assert_eq!(prepare.calls, 2);
        assert_eq!(authorization.calls, 2);
        assert_eq!(authorization.reread_calls, 2);
        assert_eq!(observation.commit_calls, 1);
        assert_eq!(provider.capability_preparation_count(), 2);
        assert_eq!(provider.interaction_count(), 1);
        assert_eq!(provider.remaining_step_count(), 0);
    }

    /// S02: the attempt gate transfers into the provider
    /// interaction and is released at its acceptance-capable boundary while
    /// the slow terminal response remains pending.
    #[tokio::test]
    async fn s02_dispatch_gate_releases_at_acceptance_boundary() {
        let (request, authorized) = prepared_fixture();
        let session = authorized.session();
        let attempt = authorized.attempt().id();
        let crossed = Arc::new(tokio::sync::Notify::new());
        let finish = Arc::new(tokio::sync::Notify::new());
        let gate = InProcessAttemptDispatchGate::default();
        let gate_probe = gate.clone();
        let mut service = ModelCallExecutionService::new(
            FixedIds::baseline(),
            FakePrepare {
                outcomes: [Ok(ready(request))].into(),
                calls: 0,
            },
            UnusedFailure,
            FakeAuthorization {
                outcomes: [Ok(authorized)].into(),
                rereads: VecDeque::new(),
                calls: 0,
                reread_calls: 0,
            },
            UnusedObservation,
            BoundaryBlockingProvider {
                crossed: Arc::clone(&crossed),
                finish: Arc::clone(&finish),
                interaction_count: 0,
            },
            gate,
            None,
        );
        {
            let execution = service.execute(session);
            tokio::pin!(execution);

            tokio::select! {
                () = crossed.notified() => {}
                result = &mut execution => panic!("provider returned before boundary probe: {result:?}"),
            }
            let after_boundary = tokio::time::timeout(
                std::time::Duration::from_millis(10),
                gate_probe.acquire(attempt),
            )
            .await
            .expect("the same-attempt gate is released at provider acceptance");
            drop(after_boundary);
            finish.notify_one();
            assert!(matches!(
                execution.as_mut().await,
                Err(ModelCallExecutionError::Provider(FakeError::Infrastructure))
            ));
        }
        let (_, _, _, _, _, provider, _, _, _, _) = service.into_parts();
        assert_eq!(provider.interaction_count, 1);
    }

    #[test]
    fn in_process_gate_serializes_the_same_attempt_but_not_distinct_attempts() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("test runtime builds");
        runtime.block_on(async {
            let gate = InProcessAttemptDispatchGate::default();
            let attempt = identity(80, TurnAttemptId::from_uuid);
            let other = identity(81, TurnAttemptId::from_uuid);
            let first = gate.acquire(attempt).await;
            let same = gate.acquire(attempt);
            tokio::pin!(same);
            assert!(
                tokio::time::timeout(std::time::Duration::ZERO, &mut same)
                    .await
                    .is_err()
            );
            let distinct =
                tokio::time::timeout(std::time::Duration::from_millis(10), gate.acquire(other))
                    .await
                    .expect("distinct attempts do not block one another");
            drop(distinct);
            drop(first);
            tokio::time::timeout(std::time::Duration::from_millis(10), same)
                .await
                .expect("same attempt proceeds after permit release");
        });
    }
}

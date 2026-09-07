use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SnapshotSelection {
    All,
    Completed {
        turn_id: CanonicalUuid,
        model_call_id: CanonicalUuid,
        terminal_entry_id: CanonicalUuid,
    },
    Failed {
        turn_id: CanonicalUuid,
        terminal_entry_id: CanonicalUuid,
    },
    Cancelled {
        turn_id: CanonicalUuid,
        terminal_entry_id: CanonicalUuid,
    },
    Refused {
        turn_id: CanonicalUuid,
        model_call_id: CanonicalUuid,
        terminal_frontier_id: CanonicalUuid,
    },
    ToolBatchProposed {
        turn_id: CanonicalUuid,
        model_call_id: CanonicalUuid,
    },
    ToolBatchResults {
        turn_id: CanonicalUuid,
        model_call_id: CanonicalUuid,
    },
    ToolReconciliation {
        turn_id: CanonicalUuid,
        tool_attempt_id: CanonicalUuid,
        terminal_frontier_id: CanonicalUuid,
    },
}

#[derive(Default)]
pub(super) struct SnapshotSelectionContext {
    requests: HashSet<CanonicalUuid>,
    cancelled_model_call: Option<CanonicalUuid>,
}

impl SnapshotSelection {
    pub(super) fn context(
        self,
        snapshot: &mut TranscriptSnapshot,
    ) -> Result<SnapshotSelectionContext, ClientError> {
        if matches!(self, Self::All) {
            return Ok(SnapshotSelectionContext::default());
        }
        let mut proposals = HashSet::new();
        let mut results = HashSet::new();
        let mut trailing_results = HashSet::new();
        let mut terminal_results = HashSet::new();
        let mut reconciliation_call = None;
        let mut reconciliation_proposals = HashSet::new();
        let mut cancelled_model_call = None;
        let mut anchor_found = false;
        for record in snapshot.replay()? {
            let record = record?;
            if let SnapshotRecord::Turn(turn) = &record {
                if matches!(
                    (self, &turn.state),
                    (
                        Self::Refused {
                            turn_id,
                            model_call_id,
                            terminal_frontier_id,
                        },
                        TurnState::Refused {
                            terminal_frontier_id: stored_frontier,
                            terminal_model_call_id: stored_call,
                            ..
                        },
                    ) if turn_id == turn.turn_id
                        && model_call_id == *stored_call
                        && terminal_frontier_id == *stored_frontier
                ) {
                    anchor_found = true;
                }
                if matches!(
                    (self, &turn.state),
                    (
                        Self::ToolReconciliation {
                            turn_id,
                            tool_attempt_id,
                            terminal_frontier_id,
                        },
                        TurnState::ToolReconciliationRequired {
                            terminal_frontier_id: stored_frontier,
                            terminal_tool_attempt_id: stored_attempt,
                            ..
                        },
                    ) if turn_id == turn.turn_id
                        && tool_attempt_id == *stored_attempt
                        && terminal_frontier_id == *stored_frontier
                ) {
                    anchor_found = true;
                }
                if let (
                    Self::Cancelled {
                        turn_id: selected_turn,
                        ..
                    },
                    TurnState::Cancelled {
                        terminal_model_call_id,
                        ..
                    },
                ) = (self, &turn.state)
                    && selected_turn == turn.turn_id
                {
                    cancelled_model_call = *terminal_model_call_id;
                }
                continue;
            }
            let SnapshotRecord::Entry(entry) = record else {
                continue;
            };
            match &entry.kind {
                SnapshotEntryKind::Marker(TranscriptEntry::AssistantToolUse {
                    turn_id,
                    model_call_id,
                    tool_request_id,
                    ..
                }) => {
                    trailing_results.clear();
                    if self.matches_tool_batch(*turn_id, *model_call_id) {
                        proposals.insert(*tool_request_id);
                        if matches!(self, Self::ToolBatchProposed { .. }) {
                            anchor_found = true;
                        }
                    }
                    if matches!(
                        self,
                        Self::ToolReconciliation {
                            turn_id: selected_turn,
                            ..
                        } if selected_turn == *turn_id
                    ) {
                        if reconciliation_call != Some(*model_call_id) {
                            reconciliation_call = Some(*model_call_id);
                            reconciliation_proposals.clear();
                        }
                        reconciliation_proposals.insert(*tool_request_id);
                    }
                }
                SnapshotEntryKind::Marker(
                    TranscriptEntry::ToolExecutionResult {
                        tool_request_id, ..
                    }
                    | TranscriptEntry::ToolDenied {
                        tool_request_id, ..
                    }
                    | TranscriptEntry::ToolInadmissible {
                        tool_request_id, ..
                    }
                    | TranscriptEntry::ToolClosed {
                        tool_request_id, ..
                    },
                ) => {
                    results.insert(*tool_request_id);
                    trailing_results.insert(*tool_request_id);
                }
                SnapshotEntryKind::Marker(TranscriptEntry::DelegationResult {
                    await_request_id,
                    mode: DelegationWaitMode::Foreground,
                    ..
                }) => {
                    results.insert(*await_request_id);
                    trailing_results.insert(*await_request_id);
                }
                _ if self.includes_terminal_marker(&entry) => {
                    anchor_found = true;
                    terminal_results.clone_from(&trailing_results);
                    trailing_results.clear();
                }
                _ => trailing_results.clear(),
            }
        }
        match self {
            Self::ToolBatchResults { .. } => {
                if proposals.is_empty()
                    || proposals.iter().any(|request| !results.contains(request))
                {
                    return Err(ClientError::Protocol(
                        "tool-result reread omitted the event's exact proposal or result set",
                    ));
                }
                Ok(SnapshotSelectionContext {
                    requests: proposals,
                    cancelled_model_call: None,
                })
            }
            Self::ToolBatchProposed { .. } if anchor_found => {
                Ok(SnapshotSelectionContext::default())
            }
            Self::Completed { .. } | Self::Failed { .. } | Self::Cancelled { .. }
                if anchor_found =>
            {
                Ok(SnapshotSelectionContext {
                    requests: terminal_results,
                    cancelled_model_call,
                })
            }
            Self::Refused { .. } if anchor_found => Ok(SnapshotSelectionContext::default()),
            Self::ToolReconciliation { .. }
                if anchor_found
                    && !reconciliation_proposals.is_empty()
                    && reconciliation_proposals
                        .iter()
                        .all(|request| results.contains(request)) =>
            {
                Ok(SnapshotSelectionContext {
                    requests: reconciliation_proposals,
                    cancelled_model_call: None,
                })
            }
            Self::ToolBatchProposed { .. } => Err(ClientError::Protocol(
                "tool-proposal reread omitted the event's exact proposal",
            )),
            Self::Completed { .. }
            | Self::Failed { .. }
            | Self::Cancelled { .. }
            | Self::Refused { .. } => Err(ClientError::Protocol(
                "terminal reread omitted the event's exact marker",
            )),
            Self::ToolReconciliation { .. } => Err(ClientError::Protocol(
                "tool reconciliation reread omitted its exact terminal result suffix",
            )),
            Self::All => Ok(SnapshotSelectionContext::default()),
        }
    }

    pub(super) fn includes(
        self,
        entry: &SnapshotEntry,
        context: &SnapshotSelectionContext,
    ) -> bool {
        match (self, &entry.kind) {
            (Self::All, _) => true,
            (
                Self::Completed {
                    turn_id,
                    model_call_id,
                    ..
                }
                | Self::ToolBatchProposed {
                    turn_id,
                    model_call_id,
                },
                SnapshotEntryKind::Text(TranscriptTextEntry::Assistant {
                    turn_id: entry_turn,
                    model_call_id: entry_call,
                }),
            ) => turn_id == *entry_turn && model_call_id == *entry_call,
            (
                Self::Cancelled { turn_id, .. },
                SnapshotEntryKind::Marker(
                    TranscriptEntry::ProviderCompaction {
                        turn_id: entry_turn,
                        model_call_id: entry_call,
                    }
                    | TranscriptEntry::ProviderReasoning {
                        turn_id: entry_turn,
                        model_call_id: entry_call,
                    },
                ),
            ) => turn_id == *entry_turn && context.cancelled_model_call == Some(*entry_call),
            (
                Self::ToolBatchProposed {
                    turn_id,
                    model_call_id,
                },
                SnapshotEntryKind::Marker(TranscriptEntry::AssistantToolUse {
                    turn_id: entry_turn,
                    model_call_id: entry_call,
                    ..
                }),
            ) => turn_id == *entry_turn && model_call_id == *entry_call,
            (
                Self::Completed {
                    turn_id,
                    model_call_id,
                    ..
                }
                | Self::Refused {
                    turn_id,
                    model_call_id,
                    ..
                }
                | Self::ToolBatchProposed {
                    turn_id,
                    model_call_id,
                }
                | Self::ToolBatchResults {
                    turn_id,
                    model_call_id,
                },
                SnapshotEntryKind::Marker(
                    TranscriptEntry::ProviderCompaction {
                        turn_id: entry_turn,
                        model_call_id: entry_call,
                    }
                    | TranscriptEntry::ProviderReasoning {
                        turn_id: entry_turn,
                        model_call_id: entry_call,
                    },
                ),
            ) => turn_id == *entry_turn && model_call_id == *entry_call,
            (
                Self::ToolBatchResults { .. }
                | Self::ToolReconciliation { .. }
                | Self::Failed { .. }
                | Self::Cancelled { .. },
                SnapshotEntryKind::Marker(
                    TranscriptEntry::ToolExecutionResult {
                        tool_request_id, ..
                    }
                    | TranscriptEntry::ToolDenied {
                        tool_request_id, ..
                    }
                    | TranscriptEntry::ToolInadmissible {
                        tool_request_id, ..
                    }
                    | TranscriptEntry::ToolClosed {
                        tool_request_id, ..
                    },
                ),
            ) => context.requests.contains(tool_request_id),
            (
                Self::ToolBatchResults { .. }
                | Self::ToolReconciliation { .. }
                | Self::Failed { .. }
                | Self::Cancelled { .. },
                SnapshotEntryKind::Marker(TranscriptEntry::DelegationResult {
                    await_request_id,
                    mode: DelegationWaitMode::Foreground,
                    ..
                }),
            ) => context.requests.contains(await_request_id),
            (
                Self::Completed { .. }
                | Self::Refused { .. }
                | Self::Failed { .. }
                | Self::Cancelled { .. },
                SnapshotEntryKind::Marker(_),
            ) => self.includes_terminal_marker(entry),
            (
                Self::Completed { .. }
                | Self::Refused { .. }
                | Self::Failed { .. }
                | Self::Cancelled { .. }
                | Self::ToolBatchProposed { .. }
                | Self::ToolBatchResults { .. }
                | Self::ToolReconciliation { .. },
                SnapshotEntryKind::User { .. } | SnapshotEntryKind::Text(_),
            ) => false,
            (
                Self::ToolBatchProposed { .. }
                | Self::ToolBatchResults { .. }
                | Self::ToolReconciliation { .. },
                SnapshotEntryKind::Marker(_),
            ) => false,
        }
    }

    fn matches_tool_batch(self, entry_turn: CanonicalUuid, entry_call: CanonicalUuid) -> bool {
        matches!(
            self,
            Self::ToolBatchProposed {
                turn_id,
                model_call_id,
            } | Self::ToolBatchResults {
                turn_id,
                model_call_id,
            } if turn_id == entry_turn && model_call_id == entry_call
        )
    }

    fn includes_terminal_marker(self, entry: &SnapshotEntry) -> bool {
        match (self, &entry.kind) {
            (
                Self::Completed {
                    turn_id,
                    terminal_entry_id,
                    ..
                },
                SnapshotEntryKind::Marker(TranscriptEntry::TurnCompleted {
                    turn_id: entry_turn,
                }),
            )
            | (
                Self::Failed {
                    turn_id,
                    terminal_entry_id,
                },
                SnapshotEntryKind::Marker(TranscriptEntry::TurnFailed {
                    turn_id: entry_turn,
                }),
            )
            | (
                Self::Cancelled {
                    turn_id,
                    terminal_entry_id,
                },
                SnapshotEntryKind::Marker(TranscriptEntry::TurnCancelled {
                    turn_id: entry_turn,
                }),
            ) => turn_id == *entry_turn && terminal_entry_id == entry.entry_id,
            (
                Self::All
                | Self::Completed { .. }
                | Self::Refused { .. }
                | Self::Failed { .. }
                | Self::Cancelled { .. }
                | Self::ToolBatchProposed { .. }
                | Self::ToolBatchResults { .. }
                | Self::ToolReconciliation { .. },
                SnapshotEntryKind::User { .. }
                | SnapshotEntryKind::Text(_)
                | SnapshotEntryKind::Marker(
                    TranscriptEntry::ModelIdentityChanged { .. }
                    | TranscriptEntry::ProviderCompaction { .. }
                    | TranscriptEntry::ProviderReasoning { .. }
                    | TranscriptEntry::DelegatedTask { .. }
                    | TranscriptEntry::DelegationMessage { .. }
                    | TranscriptEntry::DelegationResult { .. }
                    | TranscriptEntry::AssistantToolUse { .. }
                    | TranscriptEntry::ToolExecutionResult { .. }
                    | TranscriptEntry::ToolDenied { .. }
                    | TranscriptEntry::ToolInadmissible { .. }
                    | TranscriptEntry::ToolClosed { .. }
                    | TranscriptEntry::RunnerPlacementChanged { .. }
                    | TranscriptEntry::TurnCompleted { .. }
                    | TranscriptEntry::TurnFailed { .. }
                    | TranscriptEntry::TurnCancelled { .. }
                    | TranscriptEntry::Imported { .. },
                ),
            ) => false,
        }
    }
}

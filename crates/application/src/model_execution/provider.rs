use super::{
    Arc, AuthorizedModelCall, ClassifyOperatorFailure, ContextFrontierId,
    CorrelatedModelCallTerminalObservation, Future, HashMap, ModelCallCapabilityPreparation,
    ModelCallId, Mutex, OwnedMutexGuard, PreparedModelOperation, SemanticTranscriptEntryId,
    ToolRequestId, TurnAttemptId, TurnId, Weak,
};

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

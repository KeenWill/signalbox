use super::{
    AuthorizedModelCall, ClassifyOperatorFailure, CorrelatedModelCallTerminalObservation, Future,
    ModelCallCapabilityPreparation, ModelCallProvider, ModelCallTerminalObservation,
    ModelConversationMessage, OperatorFailureClass, PreparedModelOperation, ToolDefinition,
};

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

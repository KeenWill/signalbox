//! The observation stream an adapter emits while executing one operation.
//!
//! Observations are transient progress facts (ADR-0042: stream deltas are
//! never canonical transcript history); the terminal evidence returned by
//! [`crate::ModelRuntime::execute`] is the authoritative summary. Every
//! observation carries the caller's correlation identity verbatim
//! (ADR-0005).

use crate::evidence::{ExchangeFacts, FinishReason};
use crate::target::ProviderReportedModel;
use crate::tool::ToolCallProposal;
use crate::usage::TokenUsage;

/// One observation, correlated to the caller's operation identity.
#[derive(Debug, Clone, PartialEq)]
pub struct Observation<C> {
    /// The caller-supplied identity from the operation, verbatim.
    pub correlation: C,
    /// The observed fact.
    pub fact: ObservationFact,
}

/// One fact observed while executing an operation.
///
/// Boundary-progress facts ([`SendCommenced`](Self::SendCommenced),
/// [`ExchangeEstablished`](Self::ExchangeEstablished)) let the caller record
/// how far the attempt provably progressed; content facts surface transient
/// deltas and decoded proposals.
#[derive(Debug, Clone, PartialEq)]
pub enum ObservationFact {
    /// The adapter is about to hand the request to the transport. From this
    /// point the provider may have accepted it.
    SendCommenced,
    /// A correlated provider response began: proof the boundary was crossed.
    ExchangeEstablished(ExchangeFacts),
    /// The provider reported the model identity serving this exchange.
    /// Timing-sensitive under ADR-0005's mismatch rule, so it is surfaced as
    /// soon as observed rather than only in terminal evidence.
    ProviderModelReported(ProviderReportedModel),
    /// A response-text fragment.
    TextDelta {
        /// Position of the part this fragment extends, in provider part
        /// order.
        index: u32,
        /// The text fragment.
        text: String,
    },
    /// A reasoning-text fragment.
    ThinkingDelta {
        /// Position of the part this fragment extends, in provider part
        /// order.
        index: u32,
        /// The reasoning fragment.
        text: String,
    },
    /// A fragment of a tool proposal's argument JSON.
    ToolArgumentsDelta {
        /// Position of the part this fragment extends, in provider part
        /// order.
        index: u32,
        /// The raw JSON fragment.
        fragment: String,
    },
    /// A complete tool-call proposal.
    ToolCallProposed(ToolCallProposal),
    /// Provider-reported usage, possibly partial; later reports supersede
    /// per [`TokenUsage::absorb`].
    UsageReported(TokenUsage),
    /// The provider reported why generation stopped.
    FinishReported(FinishReason),
}

/// Receives observations during one execution.
///
/// Delivery is synchronous and in order; an adapter emits each observation
/// before the fact's successor is processed.
pub trait ObservationSink<C> {
    /// Receives one observation.
    fn observe(&mut self, observation: Observation<C>);
}

impl<C> ObservationSink<C> for Vec<Observation<C>> {
    fn observe(&mut self, observation: Observation<C>) {
        self.push(observation);
    }
}

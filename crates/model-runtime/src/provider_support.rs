//! Provider-neutral policy shared by named model-runtime adapters.

use std::error::Error;

use crate::{
    BoundaryLossEvidence, ExchangeFacts, LossCause, Observation, ObservationFact, ObservationSink,
    PreparationDefect, ProvenUnsentEvidence, TerminalEvidence, TokenUsage, ToolCallsAtLoss,
    TransportFacts, UnsentCause,
};

/// Maximum accepted size of one fully buffered provider response body.
// numeric-bound: ceiling - protects memory from oversized provider responses
pub const MAX_BUFFERED_PROVIDER_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// Maximum accepted aggregate size of one streamed provider response.
// numeric-bound: ceiling - protects memory from unbounded provider streams
pub const MAX_STREAMED_PROVIDER_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// How much of a provider response chunk remains inside the aggregate bound.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponsePrefixBudget {
    /// The complete chunk remains inside the bound.
    Accepted {
        /// Number of accepted bytes.
        len: usize,
    },
    /// Only a prefix remains inside the bound.
    Overflowed {
        /// Number of leading bytes that remain within the bound.
        accepted_len: usize,
    },
}

/// Applies the shared aggregate stream-response bound to one incoming chunk.
pub fn provider_response_prefix_len(current: usize, chunk: usize) -> ResponsePrefixBudget {
    let remaining = MAX_STREAMED_PROVIDER_RESPONSE_BYTES.saturating_sub(current);
    if chunk > remaining {
        ResponsePrefixBudget::Overflowed {
            accepted_len: remaining,
        }
    } else {
        ResponsePrefixBudget::Accepted { len: chunk }
    }
}

/// Constructs the typed loss used when a buffered provider body exceeds its bound.
pub fn provider_response_body_too_large() -> LossCause {
    LossCause::ResponseBodyLost(TransportFacts::new(format!(
        "response body exceeded the {MAX_BUFFERED_PROVIDER_RESPONSE_BYTES}-byte adapter limit"
    )))
}

/// Serializes one provider wire request into its final buffered representation.
pub fn serialize_provider_request(
    value: &impl serde::Serialize,
) -> Result<Vec<u8>, PreparationDefect> {
    serde_json::to_vec(value).map_err(|error| PreparationDefect::SerializationFailed {
        detail: error.to_string(),
    })
}

/// Renders a transport error and its complete source chain as retained evidence.
pub fn transport_facts_from_error(error: &(impl Error + ?Sized)) -> TransportFacts {
    let mut detail = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        detail.push_str(": ");
        detail.push_str(&cause.to_string());
        source = cause.source();
    }
    TransportFacts::new(detail)
}

/// Emits one provider observation while preserving the caller correlation.
pub fn emit_provider_observation<C: Clone>(
    correlation: &C,
    sink: &mut (dyn ObservationSink<C> + Send),
    fact: ObservationFact,
) {
    sink.observe(Observation {
        correlation: correlation.clone(),
        fact,
    });
}

/// Constructs evidence that the provider interaction provably never began.
pub fn proven_unsent_evidence(cause: UnsentCause) -> TerminalEvidence {
    TerminalEvidence::ProvenUnsent(ProvenUnsentEvidence { cause })
}

/// Constructs boundary-loss evidence before any response facts were observed.
pub fn pre_exchange_loss_evidence(cause: LossCause) -> TerminalEvidence {
    boundary_loss_evidence(cause, ExchangeFacts::default())
}

/// Constructs boundary-loss evidence with the exchange facts observed so far.
///
/// Every caller is a transport-level loss raised without a response decoder in
/// hand — a send failure, a cancelled or lost body, a status outside the
/// provider's contract — so no response material was decoded and the tool fact
/// is [`ToolCallsAtLoss::Unobserved`] rather than a claim that none opened. An
/// adapter positioned to answer it builds the evidence from its decoder
/// instead.
pub fn boundary_loss_evidence(cause: LossCause, exchange: ExchangeFacts) -> TerminalEvidence {
    TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
        cause,
        exchange,
        reported_model: None,
        finish_reported: None,
        tool_calls: ToolCallsAtLoss::Unobserved,
        usage: TokenUsage::unreported(),
    })
}

#[cfg(test)]
mod tests {
    use serde::ser::Error as _;

    use super::{
        MAX_STREAMED_PROVIDER_RESPONSE_BYTES, ResponsePrefixBudget, provider_response_prefix_len,
        serialize_provider_request,
    };
    use crate::PreparationDefect;

    struct SerializationFails;

    impl serde::Serialize for SerializationFails {
        fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            Err(S::Error::custom("fixture serialization failure"))
        }
    }

    #[test]
    fn streamed_response_budget_rejects_aggregate_overflow() {
        assert_eq!(
            provider_response_prefix_len(MAX_STREAMED_PROVIDER_RESPONSE_BYTES - 1, 1),
            ResponsePrefixBudget::Accepted { len: 1 }
        );
        assert_eq!(
            provider_response_prefix_len(MAX_STREAMED_PROVIDER_RESPONSE_BYTES - 1, 2),
            ResponsePrefixBudget::Overflowed { accepted_len: 1 }
        );
        assert_eq!(
            provider_response_prefix_len(MAX_STREAMED_PROVIDER_RESPONSE_BYTES, usize::MAX),
            ResponsePrefixBudget::Overflowed { accepted_len: 0 }
        );
    }

    #[test]
    fn serialization_failure_is_a_preparation_defect() {
        assert!(matches!(
            serialize_provider_request(&SerializationFails),
            Err(PreparationDefect::SerializationFailed { .. })
        ));
    }
}

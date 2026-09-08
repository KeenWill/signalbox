//! Provider-native rendered-input token estimation.

use std::future::Future;

use crate::{CancellationSignal, CredentialAccessFailure, ModelOperation};

/// Safe failure evidence from a provider-native input-count interaction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputTokenCountFailure {
    /// No runtime exists for the configured counting adapter.
    AdapterUnavailable,
    /// The target cannot honor the requested capabilities.
    Capability,
    /// The operation cannot be translated into the provider request.
    Translation,
    /// The translated request could not be serialized.
    Serialization,
    /// Credential delivery failed without exposing credential material.
    CredentialAccess {
        /// The credential source's closed failure classification.
        failure: CredentialAccessFailure,
    },
    /// Credential bytes cannot form the required HTTP header.
    CredentialHeader,
    /// Construction of the HTTP request failed.
    Request,
    /// Sending the request failed before a response was available.
    Transport,
    /// The provider returned an unsuccessful HTTP status.
    HttpStatus {
        /// The received HTTP status code.
        status: u16,
    },
    /// Reading the bounded response body failed.
    ResponseBody,
    /// The response exceeded the admitted JSON nesting depth.
    ResponseNesting,
    /// The response was not a valid input-count message.
    ResponseDecode,
}

impl std::fmt::Display for InputTokenCountFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AdapterUnavailable => formatter.write_str("adapter_unavailable"),
            Self::Capability => formatter.write_str("capability"),
            Self::Translation => formatter.write_str("translation"),
            Self::Serialization => formatter.write_str("serialization"),
            Self::CredentialAccess { failure } => {
                write!(formatter, "credential_access({failure:?})")
            }
            Self::CredentialHeader => formatter.write_str("credential_header"),
            Self::Request => formatter.write_str("request"),
            Self::Transport => formatter.write_str("transport"),
            Self::HttpStatus { status } => write!(formatter, "http_status({status})"),
            Self::ResponseBody => formatter.write_str("response_body"),
            Self::ResponseNesting => formatter.write_str("response_nesting"),
            Self::ResponseDecode => formatter.write_str("response_decode"),
        }
    }
}

impl std::error::Error for InputTokenCountFailure {}

/// Provider-adapter outcome for one input-count estimate request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputTokenCountOutcome<C> {
    /// The provider estimated the translated input.
    Counted {
        /// Caller-owned operation correlation.
        correlation: C,
        /// Provider-reported rendered-input estimate.
        input_tokens: u64,
    },
    /// Caller cancellation won before a complete count was available.
    Cancelled {
        /// Caller-owned operation correlation.
        correlation: C,
    },
    /// The selected adapter has no provider-native estimate operation.
    Unavailable {
        /// Caller-owned operation correlation.
        correlation: C,
    },
    /// Translation, credential access, transport, status, or response
    /// validation failed; no estimate is substituted.
    Failed {
        /// Caller-owned operation correlation.
        correlation: C,
        /// Safe provider-adapter evidence describing the failed stage.
        failure: InputTokenCountFailure,
    },
}

/// Provider adapter capable of estimating the native rendering of one
/// prospective operation without issuing a model-generation request.
pub trait ModelInputTokenCounter<C> {
    /// Performs at most one provider-native count interaction.
    fn count_input_tokens(
        &self,
        operation: ModelOperation<C>,
        cancellation: CancellationSignal,
    ) -> impl Future<Output = InputTokenCountOutcome<C>> + Send;
}

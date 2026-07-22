//! Anthropic Messages API adapter for the Layer-1 model runtime (ADR-0047).
//!
//! Under ADR-0045, translates one
//! [`signalbox_model_runtime::ModelOperation`] into an opaque, authenticated,
//! one-shot request capability before authorization, then consumes that
//! capability as at most one `POST /v1/messages` interaction — buffered or
//! SSE-streamed. It reports typed observations and terminal evidence for the
//! caller to classify under ADR-0043. Wire types are written from Anthropic's
//! public Messages API documentation.
//!
//! # One send is one request (ADR-0005)
//!
//! This adapter contains no retry, fallback, or repetition machinery, and its
//! HTTP client is configured so a single send is provably a single request:
//! redirect following is disabled and idle-connection reuse is off (see
//! [`AnthropicRuntime::new`] for the rationale and the transport facts behind
//! it).
//!
//! # Credential discipline (ADR-0017)
//!
//! The credential value is consumed inside this adapter boundary only: the
//! operation pins a non-secret reference, preparation resolves its current
//! value through the caller's `CredentialAccess` implementation exactly once
//! and scopes it to the constructed request, execution performs no second
//! lookup, provider-controlled evidence text is sanitized with that exact
//! value, and this crate performs no logging.

mod config;
mod response;
mod runtime;
mod status;
mod stream;
mod translate;
mod wire;

pub use config::AnthropicConfig;
pub use runtime::{AnthropicConstructionError, AnthropicPreparedRequest, AnthropicRuntime};

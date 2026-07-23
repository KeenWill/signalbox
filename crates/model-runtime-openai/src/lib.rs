//! OpenAI Chat Completions adapter for the Layer-1 model runtime specified
//! in `docs/spec/runtime-substrate.md`.
//!
//! It translates one [`signalbox_model_runtime::ModelOperation`] into an
//! opaque, authenticated, one-shot request capability before authorization,
//! then consumes that capability as at most one `POST /v1/chat/completions`
//! interaction — buffered or SSE-streamed. It reports typed observations and
//! terminal evidence for the caller to classify under
//! `docs/spec/model-call-execution.md`. Wire types are written from OpenAI's
//! public Chat Completions API documentation.
//!
//! # One send is one request
//!
//! No retry, fallback, or repetition machinery exists here, and the HTTP
//! client disables redirect following and idle-connection reuse so a single
//! send is provably a single request (`docs/spec/runtime-substrate.md`); see
//! [`OpenAiRuntime::new`].
//!
//! # Stream integrity
//!
//! The Chat Completions stream terminates with a literal `[DONE]` record. A
//! stream that ends any other way is explicit incomplete-stream evidence —
//! never silent success. Refusal material is never mistaken for completion;
//! because this transport cannot independently prove full request upload,
//! execution conservatively reports it as known provider-failure evidence
//! under the model-call spec rather than as a retry-safe refusal.
//!
//! # Credential discipline
//!
//! Per `docs/spec/configuration-and-credentials.md`, the operation pins a
//! non-secret reference; the runtime resolves its current value through the
//! caller's `CredentialAccess` implementation exactly once during
//! preparation, scopes it to the constructed request, and attaches it as a
//! sensitivity-marked `Authorization` header. Execution performs no second
//! lookup, provider-controlled evidence is sanitized with the captured
//! value, and this crate performs no logging.

mod config;
mod response;
mod runtime;
mod status;
mod stream;
mod translate;
mod wire;

pub use config::OpenAiConfig;
pub use runtime::{OpenAiConstructionError, OpenAiPreparedRequest, OpenAiRuntime};

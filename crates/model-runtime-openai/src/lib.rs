//! OpenAI Chat Completions adapter for the Layer-1 model runtime (ADR-0046).
//!
//! Translates one authorized [`signalbox_model_runtime::ModelOperation`] into
//! at most one `POST /v1/chat/completions` interaction — buffered or
//! SSE-streamed — and reports typed observations and terminal evidence for
//! the caller to classify under ADR-0043. Wire types are written from
//! OpenAI's public Chat Completions API documentation.
//!
//! # One send is one request (ADR-0005)
//!
//! No retry, fallback, or repetition machinery exists here, and the HTTP
//! client disables redirect following and idle-connection reuse so a single
//! send is provably a single request; see [`OpenAiRuntime::new`].
//!
//! # Stream integrity
//!
//! The Chat Completions stream terminates with a literal `[DONE]` record. A
//! stream that ends any other way is explicit incomplete-stream evidence —
//! never silent success — and refusal material is first-class refusal
//! evidence rather than an error or a silent completion.
//!
//! # Credential discipline (ADR-0017)
//!
//! The API key is attached as a sensitivity-marked `Authorization` header,
//! its debug rendering is redacted, and this crate performs no logging.

mod config;
mod response;
mod runtime;
mod status;
mod stream;
mod translate;
mod wire;

pub use config::{ApiKey, OpenAiConfig};
pub use runtime::{OpenAiConstructionError, OpenAiRuntime};

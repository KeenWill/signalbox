//! Daemon-supervised isolation for untrusted file/media adapters.
//!
//! The daemon launches one fresh local worker per operation, brokers the sole
//! verified source capability, and discards every result unless framing,
//! process exit, and cleanup all complete successfully.

mod broker;
mod protocol;
#[cfg(target_os = "linux")]
mod sandbox;
#[cfg(not(target_os = "linux"))]
mod unsupported;
mod worker;

#[cfg(target_os = "linux")]
pub use sandbox::{
    SandboxedFileMediaProcessor, SandboxedFileMediaProcessorConstructionError, WorkerBinding,
};
#[cfg(not(target_os = "linux"))]
pub use unsupported::{
    SandboxedFileMediaProcessor, SandboxedFileMediaProcessorConstructionError, WorkerBinding,
};
pub use worker::{WorkerCatalog, WorkerCatalogConstructionError, WorkerServiceError, serve_one};

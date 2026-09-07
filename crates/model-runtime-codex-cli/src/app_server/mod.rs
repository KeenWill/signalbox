//! The single-thread, single-turn app-server conversation.

pub(crate) mod classify;
pub(crate) mod client;
pub(crate) mod decode;
pub(crate) mod frame;

#[cfg(test)]
mod tests;

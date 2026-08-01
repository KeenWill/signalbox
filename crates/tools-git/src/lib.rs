//! Typed repository-local Git tools over an injected workspace root.
//!
//! Repository discovery and linked worktrees are deliberately unsupported. A
//! suite binds one direct main worktree whose .git directory is inside the
//! injected root. The local family has no remote operation.
#![allow(dead_code)]

mod construction;
mod descriptor;
mod failure;
mod index_lock;
mod layout;
mod limits;
mod packed_reference;
mod pinning;
mod reference_lock;
mod reference_read;
#[cfg(test)]
mod tests;

pub use construction::LocalGitToolsConstructionError;

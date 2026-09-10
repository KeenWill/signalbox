//! Operator workspace commands for `docs/spec/identity-and-commands.md`.

use crate::{
    DurableCommandId, GitRemoteMintId, GitRemoteName, GitRemoteUrl, GitRemoteWithdrawalId,
    WorkspaceId, WorkspaceRootPath,
};

/// Caller-supplied meaning of one workspace operator command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceOperation {
    /// Registers a root canonicalized by the operator boundary.
    Register {
        /// The canonical directory spelling.
        root: WorkspaceRootPath,
    },
    /// Mints one HTTPS or SSH destination scoped to a workspace.
    MintRemote {
        /// The registered workspace.
        workspace: WorkspaceId,
        /// The chosen remote name.
        name: GitRemoteName,
        /// The chosen HTTPS or SSH destination.
        url: GitRemoteUrl,
    },
    /// Withdraws exactly one durable mint.
    WithdrawRemote {
        /// The mint to retire.
        mint: GitRemoteMintId,
    },
}

/// One globally identified operator request; equality excludes its lookup key.
#[derive(Clone, Debug)]
pub struct WorkspaceCommand {
    command_id: DurableCommandId,
    operation: WorkspaceOperation,
}

impl WorkspaceCommand {
    /// Binds an admitted command identity to its semantic payload.
    pub const fn new(command_id: DurableCommandId, operation: WorkspaceOperation) -> Self {
        Self {
            command_id,
            operation,
        }
    }
    /// Returns the globally claimed lookup key.
    pub const fn command_id(&self) -> DurableCommandId {
        self.command_id
    }
    /// Borrows the caller's semantic payload.
    pub const fn operation(&self) -> &WorkspaceOperation {
        &self.operation
    }
}
impl PartialEq for WorkspaceCommand {
    fn eq(&self, other: &Self) -> bool {
        self.operation == other.operation
    }
}
impl Eq for WorkspaceCommand {}

/// The identity of the immutable fact produced by an operator command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceCommandResult {
    /// A registered workspace.
    Registered(WorkspaceId),
    /// A minted Git remote.
    Minted(GitRemoteMintId),
    /// A withdrawal of a Git remote mint.
    Withdrawn(GitRemoteWithdrawalId),
}

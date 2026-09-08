//! Workspace identity supply for `docs/spec/identity-and-commands.md`.

use signalbox_domain::{GitRemoteMintId, GitRemoteWithdrawalId, WorkspaceId};

/// Supplies new identities immediately before recording workspace facts.
pub trait WorkspaceIdentityGenerator: Send {
    /// Mints a workspace identity.
    fn workspace(&mut self) -> WorkspaceId;
    /// Mints a configured destination identity.
    fn remote_mint(&mut self) -> GitRemoteMintId;
    /// Mints a destination withdrawal identity.
    fn remote_withdrawal(&mut self) -> GitRemoteWithdrawalId;
}

/// Production UUIDv7 identities for workspace facts.
#[derive(Clone, Copy, Debug, Default)]
pub struct UuidV7WorkspaceIdentityGenerator;

impl WorkspaceIdentityGenerator for UuidV7WorkspaceIdentityGenerator {
    fn workspace(&mut self) -> WorkspaceId {
        WorkspaceId::from_uuid(uuid::Uuid::now_v7())
    }
    fn remote_mint(&mut self) -> GitRemoteMintId {
        GitRemoteMintId::from_uuid(uuid::Uuid::now_v7())
    }
    fn remote_withdrawal(&mut self) -> GitRemoteWithdrawalId {
        GitRemoteWithdrawalId::from_uuid(uuid::Uuid::now_v7())
    }
}

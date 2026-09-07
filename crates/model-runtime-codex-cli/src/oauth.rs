//! Isolated token-only Codex authentication homes.

use signalbox_model_runtime::{
    CancellationSignal, CredentialAccessFailure, CredentialReference, CredentialValue,
};
use std::{future::Future, pin::Pin, sync::Arc};

/// Authentication material scoped to one invocation; diagnostics omit its values.
#[derive(Debug)]
pub struct OauthCredentialMaterial {
    /// Daemon-minted access token.
    pub access_token: CredentialValue,
    /// Original identity JWT, also seeded into exact-value redaction.
    pub identity_token: CredentialValue,
    /// Non-secret account identifier harvested from the identity token.
    pub account_id: Option<String>,
}

/// Adapter callback invoked while the daemon still holds the profile row lock.
pub trait OauthCredentialInstaller: Send {
    /// Seeds exact-value redaction and writes the invocation's private authentication home.
    fn install(&mut self, material: OauthCredentialMaterial)
    -> Result<(), CredentialAccessFailure>;
}

/// One asynchronous delivery operation under daemon credential authority.
pub type OauthDeliveryFuture<'a> =
    Pin<Box<dyn Future<Output = Result<(), CredentialAccessFailure>> + Send + 'a>>;

/// Daemon-owned refresh and generation authority, independent of storage representation.
pub trait OauthCredentialProvider: Send + Sync + std::fmt::Debug {
    /// Resolves a generation and holds its row lock through the install callback.
    fn deliver<'a>(
        &'a self,
        reference: &'a CredentialReference,
        installer: &'a mut dyn OauthCredentialInstaller,
        cancellation: CancellationSignal,
    ) -> OauthDeliveryFuture<'a>;
}

#[cfg(unix)]
mod filesystem;
#[cfg(unix)]
pub(crate) use filesystem::OauthCredentialHome;
#[cfg(unix)]
pub use filesystem::OauthCredentialRoot;

#[cfg(not(unix))]
#[derive(Debug)]
pub struct OauthCredentialRoot;
#[cfg(not(unix))]
pub(crate) struct OauthCredentialHome {
    pub material: OauthCredentialMaterial,
    pub path: std::path::PathBuf,
}
#[cfg(not(unix))]
impl OauthCredentialRoot {
    pub fn open(_: &std::path::Path) -> std::io::Result<Arc<Self>> {
        Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
    }
    fn install(&self, _: OauthCredentialMaterial) -> std::io::Result<OauthCredentialHome> {
        Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
    }
}

pub(crate) struct Installer {
    pub root: Arc<OauthCredentialRoot>,
    pub home: Option<OauthCredentialHome>,
}

impl OauthCredentialInstaller for Installer {
    fn install(
        &mut self,
        material: OauthCredentialMaterial,
    ) -> Result<(), CredentialAccessFailure> {
        self.home = Some(
            self.root
                .install(material)
                .map_err(|_| CredentialAccessFailure::OauthCredentialHome)?,
        );
        Ok(())
    }
}

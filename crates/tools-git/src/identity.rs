use std::{error::Error, fmt};

use git2::Signature;

use crate::limits::MAX_IDENTITY_BYTES;

/// Injected commit author and committer identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitIdentity {
    pub(super) name: String,
    pub(super) email: String,
}

impl GitIdentity {
    /// Constructs an explicit identity without consulting Git configuration or
    /// process environment.
    pub fn try_new(
        name: impl Into<String>,
        email: impl Into<String>,
    ) -> Result<Self, InvalidGitIdentity> {
        let name = name.into();
        let email = email.into();
        if invalid_identity_part(&name) || invalid_identity_part(&email) {
            return Err(InvalidGitIdentity);
        }
        Ok(Self { name, email })
    }

    /// Borrows the configured author name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Borrows the configured author email.
    pub fn email(&self) -> &str {
        &self.email
    }

    pub(super) fn signature(&self) -> Result<Signature<'static>, git2::Error> {
        Signature::now(&self.name, &self.email)
    }
}

pub(super) fn invalid_identity_part(value: &str) -> bool {
    value.is_empty()
        || value.len() > MAX_IDENTITY_BYTES
        || value.contains('\0')
        || value.contains('\n')
        || value.contains('\r')
        || value.contains('<')
        || value.contains('>')
}

/// An injected Git identity was not safe for a commit signature.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidGitIdentity;

impl fmt::Display for InvalidGitIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid injected Git identity")
    }
}

impl Error for InvalidGitIdentity {}

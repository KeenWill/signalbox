//! Immutable program identity and explicit capability grants.

use sha2::{Digest, Sha256};

use crate::{ProgramCapability, ProgramRegistrationId};

/// Exact-byte SHA-256 content digest.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct ProgramContentDigest([u8; 32]);

impl ProgramContentDigest {
    pub fn of(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }

    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl std::fmt::Debug for ProgramContentDigest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ProgramContentDigest([digest])")
    }
}

/// An explicit set drawn only from the closed capability vocabulary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProgramGrants(Vec<ProgramCapability>);

impl ProgramGrants {
    pub fn new(capabilities: impl IntoIterator<Item = ProgramCapability>) -> Self {
        let mut grants = Vec::new();
        for capability in capabilities {
            if !grants.contains(&capability) {
                grants.push(capability);
            }
        }
        grants.sort();
        Self(grants)
    }

    pub fn capabilities(&self) -> &[ProgramCapability] {
        &self.0
    }

    pub fn contains(&self, capability: ProgramCapability) -> bool {
        self.0.contains(&capability)
    }

    pub fn permits_child(&self, child: &Self) -> bool {
        self.contains(ProgramCapability::Register)
            && child.0.iter().all(|capability| self.contains(*capability))
    }
}

/// Exact registration input supplied by the user or a program.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProgramRegistrationRequest {
    pub name: String,
    pub revision: String,
    pub source: Vec<u8>,
    pub artifact: String,
    pub grants: ProgramGrants,
}

impl ProgramRegistrationRequest {
    pub fn into_content(self) -> ProgramRegistrationContent {
        ProgramRegistrationContent {
            name: self.name,
            revision: self.revision,
            executable: ProgramExecutable::JavaScript {
                source_digest: ProgramContentDigest::of(&self.source),
                artifact: self.artifact,
            },
            grants: self.grants,
        }
    }
}

/// Registration input for a compiled native program, with no JavaScript content.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeProgramRegistrationRequest {
    pub name: String,
    pub revision: String,
    pub entry: String,
    pub native_revision: String,
    pub binary_digest: ProgramContentDigest,
    pub grants: ProgramGrants,
}

impl NativeProgramRegistrationRequest {
    pub fn into_content(self) -> ProgramRegistrationContent {
        ProgramRegistrationContent {
            name: self.name,
            revision: self.revision,
            executable: ProgramExecutable::Native {
                entry: self.entry,
                revision: self.native_revision,
                binary_digest: self.binary_digest,
            },
            grants: self.grants,
        }
    }
}

/// The exact code selected by an immutable registration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProgramExecutable {
    JavaScript {
        source_digest: ProgramContentDigest,
        artifact: String,
    },
    Native {
        entry: String,
        revision: String,
        binary_digest: ProgramContentDigest,
    },
}

/// Registration content with a language-specific executable identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProgramRegistrationContent {
    pub name: String,
    pub revision: String,
    pub executable: ProgramExecutable,
    pub grants: ProgramGrants,
}

/// One immutable registration loaded from its durable row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProgramRegistration {
    pub id: ProgramRegistrationId,
    pub content: ProgramRegistrationContent,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_grants_cannot_widen_registration_authority() {
        let parent = ProgramGrants::new([ProgramCapability::Register, ProgramCapability::Session]);
        assert!(parent.permits_child(&ProgramGrants::new([ProgramCapability::Session])));
        assert!(!parent.permits_child(&ProgramGrants::new([ProgramCapability::Judge])));
        assert!(
            !ProgramGrants::new([ProgramCapability::Session])
                .permits_child(&ProgramGrants::new([ProgramCapability::Session]))
        );
    }
}

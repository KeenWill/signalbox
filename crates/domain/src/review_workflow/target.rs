//! Review target for `docs/spec/review-workflows.md`.

use crate::ReviewTargetId;

use super::{ReviewChangeRequestNumber, ReviewKey};

/// What moving or immutable code-host subject a target snapshot represents.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ReviewTargetSubject {
    /// One change request at the target's frozen head revision.
    ChangeRequest(ReviewChangeRequestNumber),
    /// One immutable commit revision.
    Commit,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ReviewChangeRequestIdentity {
    provider: ReviewKey,
    repository: ReviewKey,
    number: ReviewChangeRequestNumber,
}

/// Canonical topology facts for one review target's immediate stack parent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewTargetParentRef {
    target: ReviewTargetId,
    provider: ReviewKey,
    repository: ReviewKey,
    head_revision: ReviewKey,
}

impl ReviewTargetParentRef {
    fn from_target(target: &ReviewTarget) -> Self {
        Self {
            target: target.id,
            provider: target.provider.clone(),
            repository: target.repository.clone(),
            head_revision: target.head_revision.clone(),
        }
    }

    /// Returns the parent target identity.
    pub const fn target(&self) -> ReviewTargetId {
        self.target
    }

    /// Borrows the parent's canonical provider key.
    pub const fn provider(&self) -> &ReviewKey {
        &self.provider
    }

    /// Borrows the parent's canonical repository key.
    pub const fn repository(&self) -> &ReviewKey {
        &self.repository
    }

    /// Borrows the parent's frozen head revision.
    pub const fn head_revision(&self) -> &ReviewKey {
        &self.head_revision
    }
}

/// One immutable review-target snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewTarget {
    id: ReviewTargetId,
    provider: ReviewKey,
    repository: ReviewKey,
    subject: ReviewTargetSubject,
    head_revision: ReviewKey,
    base_revision: Option<ReviewKey>,
    stack_parent: Option<ReviewTargetParentRef>,
    ancestry: Vec<ReviewTargetId>,
    change_request_ancestry: Vec<ReviewChangeRequestIdentity>,
}

impl ReviewTarget {
    /// Constructs a target snapshot, rejecting incomplete comparison evidence
    /// or an unauthenticated parent edge.
    pub fn try_new(
        id: ReviewTargetId,
        provider: ReviewKey,
        repository: ReviewKey,
        subject: ReviewTargetSubject,
        head_revision: ReviewKey,
        base_revision: Option<ReviewKey>,
        stack_parent: Option<&ReviewTarget>,
    ) -> Result<Self, ReviewTargetError> {
        if matches!(subject, ReviewTargetSubject::ChangeRequest(_)) && base_revision.is_none() {
            return Err(ReviewTargetError::MissingChangeRequestBase { target: id });
        }
        if let Some(parent) = stack_parent {
            if parent.id == id {
                return Err(ReviewTargetError::SelfParent { target: id });
            }
            if parent.ancestry.contains(&id) {
                return Err(ReviewTargetError::CyclicParent { target: id });
            }
            if parent.provider != provider || parent.repository != repository {
                return Err(ReviewTargetError::ForeignParent { target: id });
            }
            let Some(base_revision) = &base_revision else {
                return Err(ReviewTargetError::MissingParentBase { target: id });
            };
            if parent.head_revision != *base_revision {
                return Err(ReviewTargetError::DisconnectedParent { target: id });
            }
            if let ReviewTargetSubject::ChangeRequest(number) = subject {
                let identity = ReviewChangeRequestIdentity {
                    provider: provider.clone(),
                    repository: repository.clone(),
                    number,
                };
                if parent.change_request_ancestry.contains(&identity) {
                    return Err(ReviewTargetError::RepeatedChangeRequest { target: id });
                }
            }
        }
        let ancestry = stack_parent.map_or_else(Vec::new, |parent| {
            let mut ancestry = Vec::with_capacity(parent.ancestry.len() + 1);
            ancestry.push(parent.id);
            ancestry.extend_from_slice(&parent.ancestry);
            ancestry
        });
        let mut change_request_ancestry = Vec::new();
        if let ReviewTargetSubject::ChangeRequest(number) = subject {
            change_request_ancestry.push(ReviewChangeRequestIdentity {
                provider: provider.clone(),
                repository: repository.clone(),
                number,
            });
        }
        if let Some(parent) = stack_parent {
            change_request_ancestry.extend_from_slice(&parent.change_request_ancestry);
        }
        Ok(Self {
            id,
            provider,
            repository,
            subject,
            head_revision,
            base_revision,
            stack_parent: stack_parent.map(ReviewTargetParentRef::from_target),
            ancestry,
            change_request_ancestry,
        })
    }

    /// Reconstitutes a target snapshot after authenticating the parent
    /// identity stored on the child row against the supplied canonical parent.
    #[allow(clippy::too_many_arguments)]
    pub fn try_reconstitute(
        id: ReviewTargetId,
        provider: ReviewKey,
        repository: ReviewKey,
        subject: ReviewTargetSubject,
        head_revision: ReviewKey,
        base_revision: Option<ReviewKey>,
        stack_parent: Option<ReviewTargetId>,
        stack_parent_evidence: Option<&ReviewTarget>,
    ) -> Result<Self, ReviewTargetError> {
        if stack_parent != stack_parent_evidence.map(Self::id) {
            return Err(ReviewTargetError::ParentIdentityMismatch { target: id });
        }
        Self::try_new(
            id,
            provider,
            repository,
            subject,
            head_revision,
            base_revision,
            stack_parent_evidence,
        )
    }

    /// Returns the target identity.
    pub const fn id(&self) -> ReviewTargetId {
        self.id
    }

    /// Borrows the opaque code-host provider key.
    pub const fn provider(&self) -> &ReviewKey {
        &self.provider
    }

    /// Borrows the opaque repository key.
    pub const fn repository(&self) -> &ReviewKey {
        &self.repository
    }

    /// Returns the target subject.
    pub const fn subject(&self) -> ReviewTargetSubject {
        self.subject
    }

    /// Borrows the frozen head revision.
    pub const fn head_revision(&self) -> &ReviewKey {
        &self.head_revision
    }

    /// Borrows the optional frozen base revision.
    pub const fn base_revision(&self) -> Option<&ReviewKey> {
        self.base_revision.as_ref()
    }

    /// Returns the optional immediately preceding stack target.
    pub const fn stack_parent(&self) -> Option<&ReviewTargetParentRef> {
        self.stack_parent.as_ref()
    }

    /// Borrows the complete nearest-first canonical parent chain.
    pub fn ancestry(&self) -> &[ReviewTargetId] {
        &self.ancestry
    }
}

/// Invalid immutable target snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewTargetError {
    /// A moving change request lacks its frozen comparison revision.
    MissingChangeRequestBase {
        /// The incomplete target.
        target: ReviewTargetId,
    },
    /// The target names itself as its stack parent.
    SelfParent {
        /// The self-parented target.
        target: ReviewTargetId,
    },
    /// The complete parent chain repeats the target.
    CyclicParent {
        /// The target whose proposed chain contains itself.
        target: ReviewTargetId,
    },
    /// The parent belongs to another provider or repository.
    ForeignParent {
        /// The child target whose topology was rejected.
        target: ReviewTargetId,
    },
    /// A parented target lacks the exact comparison revision.
    MissingParentBase {
        /// The child target whose topology was rejected.
        target: ReviewTargetId,
    },
    /// The child's comparison revision is not its canonical parent's head.
    DisconnectedParent {
        /// The child target whose topology was rejected.
        target: ReviewTargetId,
    },
    /// One stack chain contains two snapshots of the same change request.
    RepeatedChangeRequest {
        /// The child snapshot whose proposed chain repeats its logical subject.
        target: ReviewTargetId,
    },
    /// The parent identity stored on the child row differs from the supplied
    /// canonical parent evidence.
    ParentIdentityMismatch {
        /// The child snapshot whose stored parent edge was rejected.
        target: ReviewTargetId,
    },
}

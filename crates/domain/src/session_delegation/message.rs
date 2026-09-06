//! Session delegation message for `docs/spec/sessions-and-transcript.md`.

use super::content::DelegationContent;
use super::provenance::DelegationProvenance;
#[cfg(doc)]
use super::relation::SessionDelegation;
use super::request::DelegationMessageRequest;
use crate::{DelegationMessageId, SessionId};

/// Direction derived from a message's exact sender in the parent/child pair.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DelegationMessageDirection {
    ParentToChild,
    ChildToParent,
}

/// Labeled parent and child endpoints used to restore one stored message.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DelegationMessageEndpoints {
    pub parent: SessionId,
    pub child: SessionId,
}

/// One immutable bidirectional message whose content is authoritative.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct DelegationMessage {
    pub(super) id: DelegationMessageId,
    pub(super) direction: DelegationMessageDirection,
    pub(super) peer: SessionId,
    pub(super) content: DelegationContent,
    pub(super) provenance: DelegationProvenance,
}

impl DelegationMessage {
    /// Reconstitutes one stored message from its canonical logical request.
    ///
    /// The containing [`SessionDelegation`] reconstitution validates the
    /// direction against the immutable parent and child endpoints.
    pub fn reconstitute(
        request: &DelegationMessageRequest,
        id: DelegationMessageId,
        direction: DelegationMessageDirection,
        endpoints: DelegationMessageEndpoints,
    ) -> Option<Self> {
        let endpoints_match = match direction {
            DelegationMessageDirection::ParentToChild => {
                request.request().session() == endpoints.parent && request.peer() == endpoints.child
            }
            DelegationMessageDirection::ChildToParent => {
                request.request().session() == endpoints.child && request.peer() == endpoints.parent
            }
        };
        endpoints_match.then(|| Self {
            id,
            direction,
            peer: request.peer(),
            content: request.content().clone(),
            provenance: DelegationProvenance::from_message(request),
        })
    }

    pub const fn id(&self) -> DelegationMessageId {
        self.id
    }
    pub const fn direction(&self) -> DelegationMessageDirection {
        self.direction
    }
    pub const fn content(&self) -> &DelegationContent {
        &self.content
    }
    pub const fn provenance(&self) -> DelegationProvenance {
        self.provenance
    }
}

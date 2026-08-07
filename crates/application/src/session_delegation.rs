//! Application projection for one durably recorded delegation message delivery.

use std::num::NonZeroU64;

use signalbox_domain::{
    DelegationEventOrdinal, DelegationMessageDirection, DelegationMessageId, ToolRequestId,
};

/// Persistence-owned evidence binding one message to its recipient delivery position.
pub trait DelegationMessageDeliveryProjection {
    fn tool_request(&self) -> ToolRequestId;
    fn message(&self) -> DelegationMessageId;
    fn direction(&self) -> DelegationMessageDirection;
    fn ordinal(&self) -> DelegationEventOrdinal;
    fn delivery_sequence(&self) -> NonZeroU64;
}

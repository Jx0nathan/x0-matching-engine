use crate::{event::Event, ids::SeqNo, reject::RejectReason};
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

/// Final outcome of processing a CommandEnvelope. Returned to the caller and
/// also broadcast for audit. The events list is the full transcript of what
/// happened — Trade, OrderAccepted, etc.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandReceipt {
    pub seq_no: SeqNo,
    pub status: CommandStatus,
    pub events: SmallVec<[Event; 4]>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommandStatus {
    Accepted,
    Rejected(RejectReason),
    Filled,
    PartiallyFilled,
    Cancelled,
}

impl CommandReceipt {
    pub fn rejected(seq_no: SeqNo, reason: RejectReason) -> Self {
        Self {
            seq_no,
            status: CommandStatus::Rejected(reason),
            events: SmallVec::new(),
        }
    }
}

//! Memory resource ownership and controller policy.
//!
//! The accounting layer has no reclaim, task lookup, or allocator callbacks.
//! Callers reserve outside resource locks and keep the returned charge in the
//! actual resource owner. Slow-path policy is deliberately separate.

pub(crate) mod abi;
pub(crate) mod accounting;
pub(crate) mod high;
pub(crate) mod io;
pub(crate) mod object;
pub(crate) mod owned;
mod ownership;
pub(crate) mod protection;
mod registry;
pub(crate) mod runtime;
pub(crate) mod socket;
pub(crate) mod stats;

pub(crate) use ownership::{account_for_task, object_for_task};

pub(crate) use accounting::{
    ChargeFailure, ChargeFailureReason, MemoryAccount, MemoryCharge, MemoryEvent, MemoryKind,
    MemoryPolicy, MemoryStat,
};

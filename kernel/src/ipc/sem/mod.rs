// SPDX-License-Identifier: GPL-2.0-or-later
//! Linux-compatible System V semaphores.
//!
//! The namespace manager serializes set values, queues and undo associations.
//! Set execution is allocation-free; syscall workflow owns preparation and waits.
//! User copies, scheduler wakeups and retired storage disposal stay outside the
//! manager lock. With SEM_UNDO, acquire manager -> entry record -> group, never
//! acquire the manager while holding the group lock.
mod abi;
mod manager;
mod operation;
mod set;

#[allow(unused_imports)] // Preserve the existing ipc::sem API across the module split.
pub use abi::{
    PosixSemBuf, PosixSemIdDs, PosixSemInfo, SemCtlCmd, SemFlags, SemId, SemKey, IPC_PRIVATE,
    SEMMNI, SEMMNS, SEMMSL, SEMOPM, SEMVMX,
};
pub use manager::SemManager;
#[allow(unused_imports)] // Named token remains available to callers of prepare_setall.
pub use manager::SemSetAllToken;
pub(crate) use set::SemWakeBatch;

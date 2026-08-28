//! Per-mm uprobe management + XOL area + breakpoint page installation (planned steps 3+4).
//!
//! This module provides uprobe registration/unregistration infrastructure, used by batch3
//! (exception dispatch) and batch4 (perf integration).
//!
//! # Key design (review findings)
//!
//! - **F8**: `uprobe_list` / `xol_pool` / `uprobe_page_state` are attached to `AddressSpace`, protected by
//!   a dedicated irqsave `SpinLock` (**not** via `inner: RwSem`); the hit path (#BP/#DB with interrupts
//!   disabled) only does `lock_irqsave` + a table lookup and never sleeps.
//! - **F1/F2**: breakpoint page installation mirrors `do_wp_page` private-file COW — under try-only source
//!   Page guards, validate and copy into an unpublished Normal page, then patch in 0xcc and use a
//!   single `set_entry` atomic swap (**never** unmap+map_phys, which would create a transient empty PTE)
//!   + rmap bookkeeping + TLB rendezvous.
//! - **F7**: a private COW copy per target mm (type `Normal`), **never** modifying the shared page-cache page
//!   (otherwise writeback would flush 0xcc and corrupt the .so).
//! - **F6 arming-order invariant**: registration strictly follows XOL slot allocation -> uprobe table
//!   entry insert -> 0xcc page publish; before 0xcc is published, any path querying that vaddr must
//!   find a ready uprobe table entry.
use crate::libs::{spinlock::SpinLock, wait_queue::WaitQueue};
use alloc::{
    collections::BTreeMap,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};

use system_error::SystemError;

use crate::{
    arch::{mm::PageMapper, MMArch},
    filesystem::{
        page_cache::PageCache,
        vfs::{file::File, IndexNode},
    },
    libs::mutex::Mutex,
    mm::{
        page::{page_manager_lock, page_reclaimer_lock, Page, PageEntry, PageFlags, PageType},
        syscall::{MapFlags, ProtFlags},
        MemoryManagementArch, PhysAddr, VirtAddr, VirtRegion, VmFlags,
    },
    process::{ProcessControlBlock, ProcessFlags},
};

use super::RwSemWriteGuard;
use super::{AddressSpace, InnerAddressSpace, LockedVMA};

use uprobe::{analyze_insn, build_xol_slot, InsnAnalysis, UPROBE_INSN_COPY_SIZE};

mod consumer;
mod definition;
mod hit_index;
mod reconcile;
mod site;
mod xol;

pub use consumer::*;
pub use definition::*;
pub use site::*;
pub use xol::*;

pub use reconcile::*;

/// Whether an enabled consumer can require executable mapping publication to
/// synchronize with uprobe installation.
pub(super) fn requires_exec_publication_barrier(
    mm: &Arc<AddressSpace>,
    file: &Arc<File>,
    flags: VmFlags,
    file_start_byte: usize,
    len: usize,
) -> bool {
    if !site::valid_probe_vma_flags(flags) || consumer::uprobe_registry_is_empty() {
        return false;
    }
    let Some(page_cache) = file.inode().page_cache() else {
        return false;
    };
    let Some(file_end) = file_start_byte.checked_add(len) else {
        return false;
    };
    let query_start = file_start_byte.saturating_sub(UPROBE_INSN_COPY_SIZE - 1);
    consumer::uprobe_registry_has_active_range_for_mm(
        mm,
        Arc::as_ptr(&page_cache) as usize,
        query_start,
        file_end,
    )
}

pub(super) fn has_active_consumers() -> bool {
    !consumer::uprobe_registry_is_empty()
}

// User-space memory management for processes

use core::{
    cmp,
    hash::Hasher,
    intrinsics::unlikely,
    ops::Add,
    sync::atomic::{compiler_fence, AtomicU64, AtomicUsize, Ordering},
};

use alloc::{
    collections::BTreeMap,
    sync::{Arc, Weak},
    vec::Vec,
};
use defer::defer;
use hashbrown::HashMap;
use hashbrown::HashSet;
use ida::IdAllocator;
use log::{error, warn};
use system_error::SystemError;

use crate::{
    arch::{mm::PageMapper, CurrentIrqArch, MMArch},
    exception::InterruptArch,
    filesystem::{
        page_cache::UnmapMappingMode,
        vfs::{
            file::{File, FileMode},
            FileType, InodeId,
        },
    },
    ipc::shm::SysVShmAttach,
    libs::{
        align::page_align_up,
        cpumask::CpuMask,
        mutex::{Mutex, MutexGuard},
        rwsem::{RwSem, RwSemReadGuard, RwSemWriteGuard},
        spinlock::SpinLock,
        wait_queue::WaitQueue,
    },
    mm::{
        mmu_gather::MmuGather,
        page::{page_manager_lock, page_reclaimer_lock},
        PhysAddr,
    },
    process::{
        cred::{capable, CAPFlags},
        resource::RLimitID,
        ProcessManager,
    },
};

use super::{
    allocator::page_frame::{
        deallocate_page_frames, PageFrameCount, PhysPageFrame, VirtPageFrame, VirtPageFrameIter,
    },
    fault::{FaultFlags, PageFaultHandler, PageFaultMessage},
    page::{EntryFlags, Flusher, Page, PageFlags, PageType},
    syscall::{MadvFlags, MapFlags, MremapFlags, ProtFlags},
    MemoryManagementArch, PageTableKind, VirtAddr, VirtRegion, VmFaultReason, VmFlags,
};
use crate::arch::mm::LockedFrameAllocator;

/// Default value for MMAP_MIN_ADDR
/// The following content from Linux 5.19:
///  This is the portion of low virtual memory which should be protected
//   from userspace allocation.  Keeping a user from writing to low pages
//   can help reduce the impact of kernel NULL pointer bugs.
//   For most ia64, ppc64 and x86 users with lots of address space
//   a value of 65536 is reasonable and should cause no problems.
//   On arm and other archs it should not be higher than 32768.
//   Programs which use vm86 functionality or have some need to map
//   this low address space will need CAP_SYS_RAWIO or disable this
//   protection by setting the value to 0.
pub const DEFAULT_MMAP_MIN_ADDR: usize = 65536;

/// Linux `security_mmap_addr()`/`cap_mmap_addr()` semantics for low fixed mappings.
///
/// Mapping below `mmap_min_addr` is denied with `EPERM` unless the caller has
/// `CAP_SYS_RAWIO` in the initial user namespace. Non-fixed hints are rounded
/// by the caller and should not enter this helper.
pub fn check_mmap_min_addr(vaddr: VirtAddr, min_vaddr: VirtAddr) -> Result<(), SystemError> {
    if vaddr < min_vaddr && !capable(CAPFlags::CAP_SYS_RAWIO) {
        return Err(SystemError::EPERM);
    }
    Ok(())
}

/// ID allocator for LockedVMA
static LOCKEDVMA_ID_ALLOCATOR: SpinLock<IdAllocator> =
    SpinLock::new(IdAllocator::new(0, usize::MAX).unwrap());

/// Global unique ID allocator for AddressSpace
/// Used to assign a globally unique and monotonically increasing ID to each address space
static ADDRESS_SPACE_ID_ALLOCATOR: AtomicU64 = AtomicU64::new(1);

pub type MmapReservationId = u64;

static MMAP_RESERVATION_ID_ALLOCATOR: AtomicU64 = AtomicU64::new(1);

mod address_space;
mod inner;
mod mapper;
mod mappings;
mod mmap;
mod mremap;
mod notifications;
mod stack;
mod vma;
mod vma_ops;

use self::{mappings::UserMappings, notifications::*, vma::VmaSplitSides};

pub use address_space::{AddressSpace, FileMappingWithFileArgs};
pub use inner::InnerAddressSpace;
pub use mapper::UserMapper;
pub use stack::UserStack;
#[allow(unused_imports)]
pub use vma::{AnonSharedMapping, LockedVMA, PhysmapParams, Provider, VMASplitResult, VMA};

//! Physical access admission for fixed-identity pages.
//!
//! A pin excludes isolation, while an Arc only keeps the old frame alive.
//! Isolation also retains the existing data write guard, so accesses already
//! protected by Page::read/write need no additional pin. This is deliberately
//! limited to one private anonymous mapping under its MM write lock: it does
//! not revoke present PTEs, stop user CPUs, or permit moving/freeing the frame.

use alloc::sync::Arc;
use core::{ops::Deref, sync::atomic::Ordering};

use crate::{
    arch::MMArch,
    libs::rwsem::RwSemWriteGuard,
    mm::{ucontext::InnerAddressSpace, MemoryManagementArch, PhysAddr, VirtAddr, VmFlags},
};

use super::{try_page_manager_lock, InnerPage, Page, PageFlags, PageType};

const ISOLATED: usize = 1usize << (usize::BITS - 1);
const MAX_PINS: usize = ISOLATED - 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PageAccessError {
    Busy,
    Overflow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PageIsolationUnsupported {
    NotNormal,
    NotIndividualFrame,
    ContiguousFrame,
    BackingOwned,
    Unevictable,
    NoMapping,
    MultipleMappings,
    UnsupportedVma,
    SpecialPageFlags,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PageIsolationError {
    Busy,
    Unsupported(PageIsolationUnsupported),
    StaleMapping,
}

/// Owned access to this exact physical frame, including after MM locks drop.
/// Not Clone: each owner must pass the atomic admission point independently.
#[derive(Debug)]
pub(crate) struct PageAccessPin {
    page: Arc<Page>,
}

impl PageAccessPin {
    pub(crate) fn page(&self) -> &Arc<Page> {
        &self.page
    }

    pub(crate) fn phys_address(&self) -> PhysAddr {
        self.page.phys_address()
    }
}

impl Drop for PageAccessPin {
    fn drop(&mut self) {
        // Return admission before the Arc field can release the final owner.
        let previous = self.page.access_state.fetch_sub(1, Ordering::Release);
        debug_assert!(previous != 0 && previous & ISOLATED == 0);
    }
}

/// Admission guard, not a completed migration or a userspace access barrier.
/// The borrowed MM write guard cannot be released while isolation survives.
#[derive(Debug)]
pub(crate) struct PageIsolationGuard<'a> {
    page: &'a Page,
    data: RwSemWriteGuard<'a, InnerPage>,
    _mm_guard: &'a RwSemWriteGuard<'a, InnerAddressSpace>,
}

impl Deref for PageIsolationGuard<'_> {
    type Target = InnerPage;

    fn deref(&self) -> &Self::Target {
        &self.data
    }
}

impl Drop for PageIsolationGuard<'_> {
    fn drop(&mut self) {
        // The data write guard still excludes direct readers until its field
        // drops. A newly admitted pin may then wait for that existing guard.
        let previous = self.page.access_state.swap(0, Ordering::Release);
        debug_assert_eq!(previous, ISOLATED);
    }
}

impl Page {
    pub(crate) fn try_pin_access(self: &Arc<Self>) -> Result<PageAccessPin, PageAccessError> {
        let mut state = self.access_state.load(Ordering::Relaxed);
        loop {
            if state & ISOLATED != 0 {
                return Err(PageAccessError::Busy);
            }
            if state == MAX_PINS {
                return Err(PageAccessError::Overflow);
            }
            match self.access_state.compare_exchange_weak(
                state,
                state + 1,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Ok(PageAccessPin { page: self.clone() }),
                Err(current) => state = current,
            }
        }
    }

    /// Try admission for a single-source, private anonymous order-0 user page.
    ///
    /// Lock order: MM write -> VMA snapshot -> PT edit -> registry identity ->
    /// Page try-write -> atomic gate. All acquisitions here are nonblocking.
    /// Never acquire another MM/PT/VMA/registry lock through the returned
    /// guard. MM-005 needs a separate migration-PTE transaction before it can
    /// release the MM lock or replace this fixed Page/PFN identity.
    pub(crate) fn try_isolate_for_migration<'a>(
        self: &'a Arc<Self>,
        space: &'a RwSemWriteGuard<'_, InnerAddressSpace>,
        address: VirtAddr,
    ) -> Result<PageIsolationGuard<'a>, PageIsolationError> {
        use PageIsolationError::{Busy, StaleMapping, Unsupported};
        use PageIsolationUnsupported as Reason;

        if address >= MMArch::USER_END_VADDR {
            return Err(StaleMapping);
        }
        let mm = space.outer_addr_space().ok_or(StaleMapping)?;
        let vma = space.mappings.contains(address).ok_or(StaleMapping)?;
        {
            let snapshot = vma.try_lock().ok_or(Busy)?;
            let unsupported = VmFlags::VM_SHARED
                | VmFlags::VM_MAYSHARE
                | VmFlags::VM_IO
                | VmFlags::VM_PFNMAP
                | VmFlags::VM_MIXEDMAP
                | VmFlags::VM_HUGETLB
                | VmFlags::VM_DONTEXPAND
                | VmFlags::VM_LOCKED
                | VmFlags::VM_LOCKONFAULT;
            if snapshot.vm_file().is_some()
                || snapshot.sysv_shm().is_some()
                || snapshot.vm_flags().intersects(unsupported)
            {
                return Err(Unsupported(Reason::UnsupportedVma));
            }
        }

        let pt_edit = mm.try_page_table_edit().ok_or(Busy)?;
        let page_address = VirtAddr::new(address.data() & !MMArch::PAGE_OFFSET_MASK);
        let (frame, flags) = space
            .user_mapper
            .utable
            .translate(page_address)
            .ok_or(StaleMapping)?;
        if frame != self.phys_address() || !flags.present() || !flags.has_user() {
            return Err(StaleMapping);
        }
        {
            let manager = try_page_manager_lock().ok_or(Busy)?;
            let registered = manager.get_registered(&frame).ok_or(StaleMapping)?;
            if !Arc::ptr_eq(&registered, self) {
                return Err(StaleMapping);
            }
        }

        let data = self.try_write().ok_or(Busy)?;
        if !matches!(data.page_type, PageType::Normal) {
            return Err(Unsupported(Reason::NotNormal));
        }
        if self.contiguous_owner.is_some() {
            return Err(Unsupported(Reason::ContiguousFrame));
        }
        if !data.individually_owns_frame {
            return Err(Unsupported(Reason::NotIndividualFrame));
        }
        if data.backing_lifetime_pins != 0 {
            return Err(Unsupported(Reason::BackingOwned));
        }
        if data.intrinsic_unevictable
            || data.flags.contains(PageFlags::PG_UNEVICTABLE)
            || data.has_mlocked_vma_refs()
        {
            return Err(Unsupported(Reason::Unevictable));
        }
        if data.flags.intersects(
            PageFlags::PG_RESERVED
                | PageFlags::PG_SLAB
                | PageFlags::PG_HEAD
                | PageFlags::PG_PRIVATE,
        ) {
            return Err(Unsupported(Reason::SpecialPageFlags));
        }
        if data.flags.contains(PageFlags::PG_WRITEBACK) {
            return Err(Busy);
        }
        match data.map_count() {
            0 => return Err(Unsupported(Reason::NoMapping)),
            1 if data.has_vma(&vma) => {}
            1 => return Err(StaleMapping),
            _ => return Err(Unsupported(Reason::MultipleMappings)),
        }
        self.access_state
            .compare_exchange(0, ISOLATED, Ordering::Acquire, Ordering::Relaxed)
            .map_err(|_| Busy)?;
        drop(pt_edit);
        Ok(PageIsolationGuard {
            page: self,
            data,
            _mm_guard: space,
        })
    }
}

#[cfg(feature = "mm_selftest")]
mod selftest;

#[cfg(feature = "mm_selftest")]
pub(crate) fn selftest_report() -> alloc::string::String {
    selftest::run_report()
}

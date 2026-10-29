//! Physical access admission for fixed-identity pages.
//!
//! A pin excludes isolation, while an Arc only keeps the old frame alive.
//! Isolation also retains the existing data write guard, so accesses already
//! protected by Page::read/write need no additional pin. This is deliberately
//! an admission primitive: the migration coordinator must separately retain
//! every relevant MM write lock, revoke PTEs, synchronize user CPUs and freeze
//! all remaining owners before moving or freeing the frame.

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
    Lru,
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
/// Strict admission borrows its MM write guard. Shared relocation instead
/// keeps the complete MM guard set in its private coordinator until commit or
/// rollback; this guard never authorizes dropping that set early.
#[derive(Debug)]
pub(crate) struct PageIsolationGuard<'page, 'mm> {
    page: &'page Arc<Page>,
    data: RwSemWriteGuard<'page, InnerPage>,
    _mm_guard: Option<&'mm RwSemWriteGuard<'mm, InnerAddressSpace>>,
    reset_gate: bool,
}

impl Deref for PageIsolationGuard<'_, '_> {
    type Target = InnerPage;

    fn deref(&self) -> &Self::Target {
        &self.data
    }
}

impl Drop for PageIsolationGuard<'_, '_> {
    fn drop(&mut self) {
        // The data write guard still excludes direct readers until its field
        // drops. A newly admitted pin may then wait for that existing guard.
        if self.reset_gate {
            let previous = self.page.access_state.swap(0, Ordering::Release);
            debug_assert_eq!(previous, ISOLATED);
        }
    }
}

/// A migration commit can relinquish the data borrow to consume the Page,
/// without admitting a new physical user or releasing the MM write guard.
/// Failure must restore registry/PTE ownership before dropping this token.
#[cfg(target_arch = "x86_64")]
pub(super) struct IsolatedPage<'a> {
    page: Option<Arc<Page>>,
    _mm_guard: Option<&'a RwSemWriteGuard<'a, InnerAddressSpace>>,
}

#[cfg(target_arch = "x86_64")]
impl<'page, 'mm> PageIsolationGuard<'page, 'mm> {
    pub(super) fn into_owned_for_commit(mut self) -> IsolatedPage<'mm> {
        let owned = IsolatedPage {
            page: Some(self.page.clone()),
            _mm_guard: self._mm_guard,
        };
        self.reset_gate = false;
        owned
    }
}

#[cfg(target_arch = "x86_64")]
impl IsolatedPage<'_> {
    pub(super) fn page(&self) -> &Arc<Page> {
        self.page.as_ref().unwrap()
    }

    pub(super) fn try_into_page(mut self) -> Result<Page, Self> {
        match Arc::try_unwrap(self.page.take().unwrap()) {
            Ok(page) => Ok(page),
            Err(page) => {
                self.page = Some(page);
                Err(self)
            }
        }
    }
}

#[cfg(target_arch = "x86_64")]
impl Drop for IsolatedPage<'_> {
    fn drop(&mut self) {
        if let Some(page) = &self.page {
            let previous = page.access_state.swap(0, Ordering::Release);
            debug_assert_eq!(previous, ISOLATED);
        }
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

    /// Isolate a fixed-frame data page after the relocation coordinator has
    /// locked and revalidated every reverse mapping. Registry/cache ownership
    /// is frozen separately by actual unique consumption before copying.
    /// No new MM lock may be taken while this guard is held.
    #[cfg(target_arch = "x86_64")]
    pub(super) fn try_isolate_shared<'page, 'mm>(
        self: &'page Arc<Self>,
    ) -> Result<PageIsolationGuard<'page, 'mm>, PageIsolationError> {
        use PageIsolationError::{Busy, Unsupported};
        use PageIsolationUnsupported as Reason;
        let data = self.try_write().ok_or(Busy)?;
        if !matches!(data.page_type, PageType::Normal | PageType::File(_)) {
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
        self.access_state
            .compare_exchange(0, ISOLATED, Ordering::Acquire, Ordering::Relaxed)
            .map_err(|_| Busy)?;
        Ok(PageIsolationGuard {
            page: self,
            data,
            _mm_guard: None,
            reset_gate: true,
        })
    }

    /// Try admission for a single-source, private anonymous order-0 user page.
    ///
    /// Lock order: MM write -> VMA snapshot -> PT edit -> registry identity ->
    /// Page try-write -> atomic gate. All acquisitions here are nonblocking.
    /// Do not acquire blocking MM/PT/VMA/registry locks through the returned
    /// guard. The page-internal migration transaction may only try PT/registry
    /// locks, with restoration before isolation ends. This guard alone does
    /// not revoke CPU translations or permit replacing the Page/PFN identity.
    pub(crate) fn try_isolate_for_migration<'page, 'mm>(
        self: &'page Arc<Self>,
        space: &'mm RwSemWriteGuard<'_, InnerAddressSpace>,
        address: VirtAddr,
    ) -> Result<PageIsolationGuard<'page, 'mm>, PageIsolationError> {
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
            _mm_guard: Some(space),
            reset_gate: true,
        })
    }
}

#[cfg(feature = "mm_selftest")]
mod selftest;

#[cfg(feature = "mm_selftest")]
pub(crate) fn selftest_report() -> alloc::string::String {
    selftest::run_report()
}

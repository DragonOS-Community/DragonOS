//! Bounded, one-page anonymous relocation under the existing MM write lock.
//!
//! Software PTE consumers wait at that lock, while hardware users are stopped
//! by a nonresident leaf and synchronous shootdown. Page identities stay fixed:
//! only consuming the unique old Page can transfer its charge and release its
//! frame. No allocator policy, background scanner or cross-MM wait queue lives
//! here. Callers must enter from sleepable task context without MM, VMA, PT,
//! Page or registry locks; the transaction owns that entire lock order.
use super::{
    access::{PageIsolationError, PageIsolationUnsupported},
    page_manager_lock, prepare_manager_capacity, try_page_manager_lock, Page, PageEntry, PageFlags,
    PageType, RawFrameExtent,
};
use crate::{
    arch::{mm::LockedFrameAllocator, CurrentIrqArch, MMArch},
    exception::{in_interrupt, InterruptArch},
    libs::mutex::Mutex,
    mm::{
        allocator::page_frame::FrameAllocator, ucontext::AddressSpace, MemoryManagementArch,
        PhysAddr, VirtAddr, VirtRegion,
    },
    process::ProcessManager,
};
use alloc::sync::Arc;
use core::{
    mem,
    sync::atomic::{AtomicUsize, Ordering},
};

// A controlled migration is the only owner of one extra physical data frame.
// Hold this through actual source/destination destruction, not just PTE commit.
static MIGRATION: Mutex<()> = Mutex::new(());
// x86 software bit; neither hardware PRESENT nor software PROT_NONE/GLOBAL.
const MIGRATION_ENTRY: usize = 1 << 9;
const _: () = {
    let entry = PageEntry::<MMArch>::from_usize(MIGRATION_ENTRY);
    assert!(MIGRATION_ENTRY & !(MMArch::ENTRY_FLAG_DIRTY | MMArch::ENTRY_FLAG_ACCESSED) != 0);
    assert!(!entry.present());
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MigrationError {
    Busy,
    Unsupported(PageIsolationUnsupported),
    InvalidMapping,
    NoMemory,
    Injected,
}
impl From<PageIsolationError> for MigrationError {
    fn from(error: PageIsolationError) -> Self {
        match error {
            PageIsolationError::Busy => Self::Busy,
            PageIsolationError::Unsupported(reason) => Self::Unsupported(reason),
            PageIsolationError::StaleMapping => Self::InvalidMapping,
        }
    }
}

/// Diagnostic identities, not permission to access either PFN after return.
#[derive(Debug, Clone, Copy)]
pub(crate) struct MigrationResult {
    pub(crate) old_phys: PhysAddr,
    pub(crate) new_phys: PhysAddr,
}

#[cfg(feature = "mm_selftest")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MigrationStage {
    Prepared,
    Revoked,
    Flushed,
    Copied,
    BeforeCommit,
}

#[cfg(feature = "mm_selftest")]
pub(crate) mod selftest;

pub(crate) fn migrate_private_page(
    mm: &Arc<AddressSpace>,
    address: VirtAddr,
) -> Result<MigrationResult, MigrationError> {
    migrate_inner(
        mm,
        address,
        #[cfg(feature = "mm_selftest")]
        None,
    )
}

#[cfg(feature = "mm_selftest")]
pub(crate) fn migrate_with_control(
    mm: &Arc<AddressSpace>,
    address: VirtAddr,
    control: &selftest::Control,
) -> Result<MigrationResult, MigrationError> {
    migrate_inner(mm, address, Some(control))
}

/// Owns restoration, including stale translations, until a no-failure commit.
/// The caller retains the MM write guard (and source ownership) for its life.
struct RevokedLeaf<'a> {
    mm: &'a Arc<AddressSpace>,
    address: VirtAddr,
    slot: &'a AtomicUsize,
    original: usize,
    armed: bool,
}
impl RevokedLeaf<'_> {
    fn rollback(&mut self) {
        if self.armed {
            // Pure anonymous tables cannot be changed outside MM write. The
            // same stable leaf is restored without allocation or taking a
            // blocking PT/registry lock in the rollback path.
            self.slot.store(self.original, Ordering::SeqCst);
            self.flush();
            self.armed = false;
        }
    }
    fn flush(&self) {
        self.mm.flush_tlb_range(
            self.address,
            self.address + MMArch::PAGE_SIZE,
            MMArch::PAGE_SHIFT as u8,
            false,
        );
    }
    fn publish(&mut self, physical: PhysAddr) {
        let flags = PageEntry::<MMArch>::from_usize(self.original).flags();
        self.slot.store(
            PageEntry::<MMArch>::new(physical, flags).data(),
            Ordering::SeqCst,
        );
        self.armed = false;
    }
}
impl Drop for RevokedLeaf<'_> {
    fn drop(&mut self) {
        self.rollback();
    }
}

fn migrate_inner(
    mm: &Arc<AddressSpace>,
    address: VirtAddr,
    #[cfg(feature = "mm_selftest")] control: Option<&selftest::Control>,
) -> Result<MigrationResult, MigrationError> {
    use MigrationError::{Busy, InvalidMapping, NoMemory, Unsupported};
    if !CurrentIrqArch::is_irq_enabled()
        || in_interrupt()
        || ProcessManager::current_pcb().preempt_count() != 0
    {
        return Err(Busy);
    }
    if address >= MMArch::USER_END_VADDR {
        return Err(InvalidMapping);
    }
    let address = VirtAddr::new(address.data() & !MMArch::PAGE_OFFSET_MASK);
    let _budget = MIGRATION.lock();
    // All fallible physical/metadata preparation precedes the MM critical
    // section and cannot recursively recover/reclaim this address space.
    let frame = unsafe { LockedFrameAllocator.allocate_one() }.ok_or(NoMemory)?;
    let mut raw = RawFrameExtent {
        next: frame,
        remaining: 1,
    };
    let destination = unsafe { MMArch::phys_2_virt(frame) }.ok_or(InvalidMapping)?;
    raw.remaining = 0; // Page constructor owns this frame even on Arc failure.
    let mut target =
        Page::try_new(frame, PageType::Normal, PageFlags::empty()).map_err(|_| NoMemory)?;
    let mut capacity = prepare_manager_capacity().map_err(|_| NoMemory)?;
    let mut retired_capacity = None;
    // Capacity installation may rehash every registered Page. Do it before
    // acquiring this MM or Page lock; the one-page commit must stay O(1).
    {
        let mut manager = page_manager_lock();
        if manager.phys2page.len() == manager.phys2page.capacity() {
            if capacity
                .as_ref()
                .is_none_or(|map| map.capacity() <= manager.phys2page.len())
            {
                return Err(Busy);
            }
            let mut prepared = capacity.take().unwrap();
            for (key, page) in manager.phys2page.drain() {
                assert!(prepared.insert(key, page).is_none());
            }
            mem::swap(&mut manager.phys2page, &mut prepared);
            retired_capacity = Some(prepared);
        }
    }
    drop(retired_capacity);
    drop(capacity);
    #[cfg(feature = "mm_selftest")]
    if let Some(control) = control {
        control.at(MigrationStage::Prepared, mm, address)?;
    }

    let space = mm.write();
    if mm.is_torn_down()
        || mm.is_oom_unstable()
        || space
            .mappings
            .first_reservation_conflict(VirtRegion::new(address, MMArch::PAGE_SIZE))
            .is_some()
    {
        return Err(InvalidMapping);
    }
    let (source, slot) = {
        let _pt = mm.try_page_table_edit().ok_or(Busy)?;
        // Reject huge parents before the ordinary translate walker can follow
        // a data frame as if it were a lower page table.
        let slot =
            unsafe { space.user_mapper.utable.migration_leaf(address) }.ok_or(InvalidMapping)?;
        let entry = PageEntry::<MMArch>::from_usize(slot.load(Ordering::Acquire));
        if !entry.flags().present() || !entry.flags().has_user() {
            return Err(InvalidMapping);
        }
        let physical = entry.address().map_err(|_| InvalidMapping)?;
        let manager = try_page_manager_lock().ok_or(Busy)?;
        let source = manager.get_registered(&physical).ok_or(InvalidMapping)?;
        (source, slot)
    };
    let old_phys = source.phys_address();
    let source_address = unsafe { MMArch::phys_2_virt(old_phys) }.ok_or(InvalidMapping)?;
    let isolation = source.try_isolate_for_migration(&space, address)?;
    if isolation.flags().contains(PageFlags::PG_LRU) {
        return Err(Unsupported(PageIsolationUnsupported::Lru));
    }
    // Declare restoration after the data guard: before ownership conversion,
    // any early return restores and flushes while the source remains isolated.
    let mut revoked = {
        let _pt = mm.try_page_table_edit().ok_or(Busy)?;
        #[cfg(feature = "mm_selftest")]
        if let Some(control) = control {
            control.before_revoke();
        }
        let original = slot.swap(MIGRATION_ENTRY, Ordering::SeqCst);
        RevokedLeaf {
            mm,
            address,
            slot,
            original,
            armed: true,
        }
    };
    let captured = PageEntry::<MMArch>::from_usize(revoked.original);
    if captured.address() != Ok(old_phys)
        || !captured.flags().present()
        || !captured.flags().has_user()
    {
        return Err(InvalidMapping);
    }
    #[cfg(feature = "mm_selftest")]
    if let Some(control) = control {
        control.at(MigrationStage::Revoked, mm, address)?;
    }
    revoked.flush();
    #[cfg(feature = "mm_selftest")]
    if let Some(control) = control {
        control.at(MigrationStage::Flushed, mm, address)?;
    }
    unsafe {
        destination
            .as_ptr::<u8>()
            .copy_from_nonoverlapping(source_address.as_ptr::<u8>(), MMArch::PAGE_SIZE);
    }
    #[cfg(feature = "mm_selftest")]
    if let Some(control) = control {
        control.at(MigrationStage::Copied, mm, address)?;
    }
    #[cfg(feature = "mm_selftest")]
    if let Some(control) = control {
        control.at(MigrationStage::BeforeCommit, mm, address)?;
    }

    let pt = mm.try_page_table_edit().ok_or(Busy)?;
    let mut manager = try_page_manager_lock().ok_or(Busy)?;
    // A competing birth may consume the room prepared before MM admission.
    // Retry later rather than allocate or rehash under MM/Page/PT locks.
    if manager.phys2page.len() == manager.phys2page.capacity() {
        return Err(Busy);
    }
    if manager.phys2page.contains_key(&frame)
        || !manager
            .phys2page
            .get(&old_phys)
            .is_some_and(|page| Arc::ptr_eq(page, &source))
    {
        return Err(InvalidMapping);
    }
    let token = isolation.into_owned_for_commit();
    drop(source);
    let registered = manager
        .phys2page
        .remove(&old_phys)
        .expect("validated source registry");
    drop(registered); // token still pins the source, no Page/frame destructor.
    let mut old = match token.try_into_page() {
        Ok(page) => page,
        Err(token) => {
            #[cfg(feature = "mm_selftest")]
            if let Some(control) = control {
                control.extra_owner_rejected();
            }
            // Restore the registry before any token can disappear. Its source
            // frame then remains owned even if external references drop now.
            assert!(manager
                .phys2page
                .insert(old_phys, token.page().clone())
                .is_none());
            drop(manager);
            drop(pt);
            revoked.rollback();
            drop(token);
            return Err(Busy);
        }
    };
    // Unique old Page is the irreversible commit point. Through registry/PTE
    // publication below there are no allocations, fallible calls, additional
    // locks or test injection. Flush and destruction follow outside PT/registry.
    let new = Arc::get_mut(&mut target).expect("unpublished destination has one owner");
    new.memory_charge = old.memory_charge.take();
    let old_inner = old.inner.get_mut();
    let new_inner = new.inner.get_mut();
    new_inner.vma_set = mem::replace(&mut old_inner.vma_set, super::PageVmaSet::new());
    new_inner.flags = mem::replace(&mut old_inner.flags, PageFlags::empty());
    new_inner.memory_stats = old_inner.memory_stats.take();
    assert!(manager.phys2page.insert(frame, target).is_none());
    revoked.publish(frame);
    drop(manager);
    drop(pt);
    // Also orders publication before waiters resume. It is not the evidence
    // used to justify the earlier copy or destruction of the source frame.
    revoked.flush();
    drop(revoked);
    drop(space);
    drop(old); // actual independently owned frame free; receipt already moved.
    #[cfg(feature = "mm_selftest")]
    if let Some(control) = control {
        control.committed(old_phys, frame);
    }
    Ok(MigrationResult {
        old_phys,
        new_phys: frame,
    })
}

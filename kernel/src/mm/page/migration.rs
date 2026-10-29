//! Fixed-PFN data-page relocation across all reverse mappings and cache owners.
//! One transaction owns preparation, revocation, reference freezing and rollback.
//! Each Page still denotes one immutable physical frame; unknown owners refuse
//! relocation instead of silently acquiring a different physical identity.
use super::{
    access::{PageIsolationError, PageIsolationUnsupported},
    page_manager_lock, page_reclaimer_lock, prepare_lru_capacity, prepare_manager_capacity,
    try_page_manager_lock, try_page_reclaimer_lock, Page, PageEntry, PageFlags, PageMapping,
    PageType, RawFrameExtent,
};
use crate::{
    arch::{mm::LockedFrameAllocator, CurrentIrqArch, MMArch},
    exception::{in_interrupt, InterruptArch},
    filesystem::page_cache::PageCache,
    libs::mutex::Mutex,
    mm::{ucontext::AddressSpace, MemoryManagementArch, PhysAddr, VirtAddr, VirtRegion, VmFlags},
    process::ProcessManager,
};
use alloc::{sync::Arc, vec::Vec};
use core::{
    mem,
    sync::atomic::{AtomicUsize, Ordering},
};

static MIGRATION: Mutex<()> = Mutex::new(());

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

/// Diagnostic identities, never permission to dereference either PFN.
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
    CopyReady,
    BeforeCommit,
}
#[cfg(feature = "mm_selftest")]
pub(crate) mod selftest;

#[derive(Clone, Copy)]
enum Scope {
    Private,
    Data,
}

fn resident_leaf(entry: PageEntry<MMArch>, scope: Scope) -> bool {
    match scope {
        Scope::Private => entry.flags().present() && entry.flags().has_user(),
        Scope::Data => entry.present() && (entry.protnone() || entry.flags().has_user()),
    }
}

fn context_allowed() -> bool {
    CurrentIrqArch::is_irq_enabled()
        && !in_interrupt()
        && ProcessManager::current_pcb().preempt_count() == 0
}

pub(crate) fn migrate_private_page(
    mm: &Arc<AddressSpace>,
    address: VirtAddr,
) -> Result<MigrationResult, MigrationError> {
    migrate_address(
        mm,
        address,
        Scope::Private,
        #[cfg(feature = "mm_selftest")]
        None,
    )
}
pub(crate) fn migrate_data_page(
    mm: &Arc<AddressSpace>,
    address: VirtAddr,
) -> Result<MigrationResult, MigrationError> {
    migrate_address(
        mm,
        address,
        Scope::Data,
        #[cfg(feature = "mm_selftest")]
        None,
    )
}

/// Relocate an existing cache page even when no user PTE references it.
/// Lookup owns only a fixed-PFN Page Arc; cache identity is frozen at commit.
pub(crate) fn migrate_cached_page(
    cache: &Arc<PageCache>,
    index: usize,
) -> Result<MigrationResult, MigrationError> {
    if !context_allowed() {
        return Err(MigrationError::Busy);
    }
    let source = cache.try_migration_source(index).map_err(|error| {
        if error == system_error::SystemError::ENOENT {
            MigrationError::InvalidMapping
        } else {
            MigrationError::Busy
        }
    })?;
    relocate(
        source,
        None,
        Scope::Data,
        usize::MAX,
        #[cfg(feature = "mm_selftest")]
        None,
    )
}

/// Relocate one registered physical candidate without retaining a second
/// observer Arc. The caller owns target-range isolation; this transaction
/// owns its one destination frame and all mapping/ownership handoff.
/// Mapping limits and contention refuse bounded work before PTE changes.
pub(crate) fn migrate_frame(
    physical: PhysAddr,
    max_mappings: usize,
) -> Result<MigrationResult, MigrationError> {
    use MigrationError::{Busy, InvalidMapping};
    if !context_allowed() {
        return Err(Busy);
    }
    if !physical.check_aligned(MMArch::PAGE_SIZE) {
        return Err(InvalidMapping);
    }
    let source = {
        let manager = try_page_manager_lock().ok_or(Busy)?;
        manager.get_registered(&physical).ok_or(InvalidMapping)?
    };
    relocate(
        source,
        None,
        Scope::Data,
        max_mappings,
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
    migrate_address(mm, address, Scope::Private, Some(control))
}
#[cfg(feature = "mm_selftest")]
pub(crate) fn migrate_data_with_control(
    mm: &Arc<AddressSpace>,
    address: VirtAddr,
    control: &selftest::Control,
) -> Result<MigrationResult, MigrationError> {
    migrate_address(mm, address, Scope::Data, Some(control))
}

fn migrate_address(
    mm: &Arc<AddressSpace>,
    address: VirtAddr,
    scope: Scope,
    #[cfg(feature = "mm_selftest")] control: Option<&selftest::Control>,
) -> Result<MigrationResult, MigrationError> {
    use MigrationError::{Busy, InvalidMapping};
    if !context_allowed() {
        return Err(Busy);
    }
    if address >= MMArch::USER_END_VADDR {
        return Err(InvalidMapping);
    }
    let address = VirtAddr::new(address.data() & !MMArch::PAGE_OFFSET_MASK);
    let source = {
        let space = mm.read();
        let _pt = mm.try_page_table_edit().ok_or(Busy)?;
        let slot =
            unsafe { space.user_mapper.utable.migration_leaf(address) }.ok_or(InvalidMapping)?;
        let entry = PageEntry::<MMArch>::from_usize(slot.load(Ordering::Acquire));
        if !resident_leaf(entry, scope) {
            return Err(InvalidMapping);
        }
        try_page_manager_lock()
            .ok_or(Busy)?
            .get_registered(&entry.address().map_err(|_| InvalidMapping)?)
            .ok_or(InvalidMapping)?
    };
    relocate(
        source,
        Some((mm, address)),
        scope,
        usize::MAX,
        #[cfg(feature = "mm_selftest")]
        control,
    )
}

struct Leaf<'a> {
    mm_index: usize,
    address: VirtAddr,
    slot: &'a AtomicUsize,
    original: usize,
    revoked: bool,
}
struct Group<'a> {
    mm: &'a Arc<AddressSpace>,
    range: Option<VirtRegion>,
}
/// Leaves are stable while every contributing MM write guard is retained.
/// All vector capacity is reserved before the first PTE change.
struct Revoked<'a> {
    leaves: Vec<Leaf<'a>>,
    groups: Vec<Group<'a>>,
    armed: bool,
}
impl Revoked<'_> {
    fn touch(&mut self, mm: usize, address: VirtAddr) {
        let end = address + MMArch::PAGE_SIZE;
        let range = &mut self.groups[mm].range;
        *range = Some(match *range {
            Some(previous) => {
                let start = address.min(previous.start());
                VirtRegion::new(start, end.max(previous.end()).data() - start.data())
            }
            None => VirtRegion::new(address, MMArch::PAGE_SIZE),
        });
    }
    fn flush(&self) {
        for group in &self.groups {
            if let Some(range) = group.range {
                group.mm.flush_tlb_range(
                    range.start(),
                    range.end(),
                    MMArch::PAGE_SHIFT as u8,
                    false,
                );
            }
        }
    }
    fn rollback(&mut self) {
        if self.armed {
            for leaf in &self.leaves {
                if leaf.revoked {
                    leaf.slot.store(leaf.original, Ordering::SeqCst);
                }
            }
            self.flush();
            self.armed = false;
        }
    }
    fn publish(&self, frame: PhysAddr) {
        for leaf in &self.leaves {
            let flags = PageEntry::<MMArch>::from_usize(leaf.original).flags();
            leaf.slot.store(
                PageEntry::<MMArch>::new(frame, flags).data(),
                Ordering::SeqCst,
            );
        }
    }
}
impl Drop for Revoked<'_> {
    fn drop(&mut self) {
        self.rollback();
    }
}

/// Reserve global-index growth outside every MM and Page lock. Removed keys
/// can leave tombstones, so the commit also verifies one spare insertion slot.
fn prepare_registries(lru: bool) -> Result<(), MigrationError> {
    use MigrationError::{Busy, NoMemory};
    let mut prepared = prepare_manager_capacity().map_err(|_| NoMemory)?;
    let retired = {
        let mut manager = page_manager_lock();
        if manager.phys2page.len() == manager.phys2page.capacity() {
            let mut table = prepared
                .take()
                .filter(|p| p.capacity() > manager.phys2page.len())
                .ok_or(Busy)?;
            for (key, page) in manager.phys2page.drain() {
                assert!(table.insert(key, page).is_none());
            }
            Some(mem::replace(&mut manager.phys2page, table))
        } else {
            None
        }
    };
    drop(retired);
    drop(prepared);
    if lru {
        let mut prepared = prepare_lru_capacity().map_err(|_| NoMemory)?;
        let retired = {
            let mut reclaimer = page_reclaimer_lock();
            if reclaimer.lru.len() == reclaimer.lru.capacity() {
                let table = prepared
                    .take()
                    .filter(|p| p.capacity() > reclaimer.lru.len())
                    .ok_or(Busy)?;
                Some(reclaimer.lru.install_capacity(table))
            } else {
                None
            }
        };
        drop(retired);
    }
    Ok(())
}

fn relocate(
    source: Arc<Page>,
    origin: Option<(&Arc<AddressSpace>, VirtAddr)>,
    scope: Scope,
    max_mappings: usize,
    #[cfg(feature = "mm_selftest")] control: Option<&selftest::Control>,
) -> Result<MigrationResult, MigrationError> {
    use MigrationError::{Busy, InvalidMapping, NoMemory, Unsupported};
    let _budget = MIGRATION.try_lock().map_err(|_| Busy)?;
    let old_phys = source.phys_address();
    let source_address = unsafe { MMArch::phys_2_virt(old_phys) }.ok_or(InvalidMapping)?;
    let mut mappings: Vec<PageMapping> = Vec::new();
    let count = source.try_read().ok_or(Busy)?.map_count();
    if count > max_mappings {
        return Err(Busy);
    }
    mappings.try_reserve(count).map_err(|_| NoMemory)?;
    let (cache, was_lru) = {
        let data = source.try_read().ok_or(Busy)?;
        if data.map_count() > max_mappings || data.map_count() > mappings.capacity() {
            return Err(Busy);
        }
        mappings.extend(data.mappings().cloned());
        let cache = match data.page_type() {
            PageType::File(info) => {
                Some((info.page_cache.upgrade().ok_or(InvalidMapping)?, info.index))
            }
            PageType::Normal => None,
            _ => return Err(Unsupported(PageIsolationUnsupported::NotNormal)),
        };
        (cache, data.flags().contains(PageFlags::PG_LRU))
    };
    if mappings.is_empty() && cache.is_none() {
        return Err(Unsupported(PageIsolationUnsupported::NoMapping));
    }
    mappings.sort_unstable_by_key(|mapping| mapping.vma.id());
    let mut mapping_mms = Vec::new();
    mapping_mms
        .try_reserve(mappings.len())
        .map_err(|_| NoMemory)?;
    for mapping in &mappings {
        mapping_mms.push(
            mapping
                .vma
                .try_lock()
                .ok_or(Busy)?
                .address_space()
                .and_then(|weak| weak.upgrade())
                .ok_or(Busy)?,
        );
    }
    let mut mms = Vec::new();
    mms.try_reserve(mapping_mms.len() + usize::from(origin.is_some()))
        .map_err(|_| NoMemory)?;
    mms.extend(mapping_mms.iter().cloned());
    if let Some((mm, _)) = origin {
        mms.push(mm.clone());
    }
    mms.sort_unstable_by_key(|mm| mm.id());
    mms.dedup_by_key(|mm| mm.id());
    let mut prepared_leaves = Vec::new();
    prepared_leaves
        .try_reserve(mappings.len())
        .map_err(|_| NoMemory)?;
    let mut prepared_groups = Vec::new();
    prepared_groups
        .try_reserve(mms.len())
        .map_err(|_| NoMemory)?;
    prepared_groups.extend(mms.iter().map(|mm| Group { mm, range: None }));
    let mut prepared_pt = Vec::new();
    prepared_pt.try_reserve(mms.len()).map_err(|_| NoMemory)?;
    prepare_registries(was_lru)?;
    let frame =
        unsafe { LockedFrameAllocator::allocate_migration_target_no_reclaim() }.ok_or(NoMemory)?;
    let mut raw = RawFrameExtent {
        next: frame,
        remaining: 1,
    };
    let destination = unsafe { MMArch::phys_2_virt(frame) }.ok_or(InvalidMapping)?;
    raw.remaining = 0;
    let mut target =
        Some(Page::try_new(frame, PageType::Normal, PageFlags::empty()).map_err(|_| NoMemory)?);
    #[cfg(feature = "mm_selftest")]
    if let (Some(control), Some((mm, address))) = (control, origin) {
        control.at(MigrationStage::Prepared, mm, address)?;
    }

    let mut spaces = Vec::new();
    spaces.try_reserve(mms.len()).map_err(|_| NoMemory)?;
    // No blocking MM acquisition while holding another MM or Page resource.
    for mm in &mms {
        let space = mm.try_write().ok_or(Busy)?;
        if mm.is_torn_down() || mm.is_oom_unstable() {
            return Err(InvalidMapping);
        }
        spaces.push(space);
    }
    if let Some((mm, address)) = origin {
        if !mappings
            .iter()
            .zip(&mapping_mms)
            .any(|(mapping, actual)| mapping.address == address && Arc::ptr_eq(actual, mm))
        {
            return Err(InvalidMapping);
        }
    }
    let origin_index = origin.map(|(mm, _)| {
        mms.binary_search_by_key(&mm.id(), |other| other.id())
            .unwrap()
    });
    let isolation = if matches!(scope, Scope::Private) {
        let (_, address) = origin.ok_or(InvalidMapping)?;
        source.try_isolate_for_migration(&spaces[origin_index.unwrap()], address)?
    } else {
        source.try_isolate_shared()?
    };
    if matches!(scope, Scope::Private) && isolation.flags().contains(PageFlags::PG_LRU) {
        return Err(Unsupported(PageIsolationUnsupported::Lru));
    }
    let mut revoked = Revoked {
        leaves: prepared_leaves,
        groups: prepared_groups,
        armed: true,
    };
    if isolation.map_count() != mappings.len()
        || isolation.mappings().any(|current| {
            match mappings.binary_search_by_key(&current.vma.id(), |item| item.vma.id()) {
                Ok(index) => mappings[index].address != current.address,
                Err(_) => true,
            }
        })
    {
        return Err(Busy);
    }
    // Full rmap is stable under Page exclusion. Each source MM excludes every
    // existing file-rmap walker as well as faults, fork, unmap and teardown.
    for (mapping, mm) in mappings.iter().zip(&mapping_mms) {
        let index = mms
            .binary_search_by_key(&mm.id(), |item| item.id())
            .unwrap();
        let space = &spaces[index];
        let vma = mapping.vma.try_lock().ok_or(Busy)?;
        if mapping.address >= MMArch::USER_END_VADDR
            || !vma.region().contains(mapping.address)
            || vma
                .address_space()
                .and_then(|weak| weak.upgrade())
                .is_none_or(|actual| !Arc::ptr_eq(&actual, mm))
            || !space.mappings.indexes_vma(mapping.address, &mapping.vma)
            || space
                .mappings
                .first_reservation_conflict(VirtRegion::new(mapping.address, MMArch::PAGE_SIZE))
                .is_some()
        {
            return Err(InvalidMapping);
        }
        if vma.vm_flags().intersects(
            VmFlags::VM_IO
                | VmFlags::VM_PFNMAP
                | VmFlags::VM_MIXEDMAP
                | VmFlags::VM_HUGETLB
                | VmFlags::VM_DONTEXPAND
                | VmFlags::VM_LOCKED
                | VmFlags::VM_LOCKONFAULT,
        ) {
            return Err(Unsupported(PageIsolationUnsupported::UnsupportedVma));
        }
        drop(vma);
        let _pt = mm.try_page_table_edit().ok_or(Busy)?;
        let slot = unsafe { space.user_mapper.utable.migration_leaf(mapping.address) }
            .ok_or(InvalidMapping)?;
        let entry = PageEntry::<MMArch>::from_usize(slot.load(Ordering::Acquire));
        if entry.address() != Ok(old_phys) || !resident_leaf(entry, scope) {
            return Err(InvalidMapping);
        }
        // No allocation: capacity was reserved before all MM locks.
        revoked.leaves.push(Leaf {
            mm_index: index,
            address: mapping.address,
            slot,
            original: entry.data(),
            revoked: false,
        });
    }
    #[cfg(feature = "mm_selftest")]
    if let Some(control) = control {
        control.before_revoke();
    }
    for index in 0..revoked.leaves.len() {
        let leaf = &revoked.leaves[index];
        let _pt = mms[leaf.mm_index].try_page_table_edit().ok_or(Busy)?;
        let original = leaf.slot.swap(MIGRATION_ENTRY, Ordering::SeqCst);
        let (mm_index, address) = (leaf.mm_index, leaf.address);
        revoked.leaves[index].original = original;
        revoked.leaves[index].revoked = true;
        revoked.touch(mm_index, address);
        let entry = PageEntry::<MMArch>::from_usize(original);
        if entry.address() != Ok(old_phys) || !resident_leaf(entry, scope) {
            return Err(InvalidMapping);
        }
        #[cfg(feature = "mm_selftest")]
        if let (Some(control), Some((mm, address))) = (control, origin) {
            control.at(MigrationStage::Revoked, mm, address)?;
        }
    }
    revoked.flush();
    #[cfg(feature = "mm_selftest")]
    if let (Some(control), Some((mm, address))) = (control, origin) {
        control.at(MigrationStage::Flushed, mm, address)?;
        control.at(MigrationStage::CopyReady, mm, address)?;
    }
    // Acquire all final guards before detaching any owner. No lock acquisition
    // or fallible operation follows successful source consumption.
    let mut pt_guards = prepared_pt;
    for mm in &mms {
        pt_guards.push(mm.try_page_table_edit().ok_or(Busy)?);
    }
    let mut manager = try_page_manager_lock().ok_or(Busy)?;
    let lru = isolation.flags().contains(PageFlags::PG_LRU);
    let mut reclaimer = if lru {
        Some(try_page_reclaimer_lock().ok_or(Busy)?)
    } else {
        None
    };
    let mut cache_guard = match &cache {
        Some((cache, index)) => Some(cache.try_migration_page(*index, &source).ok_or(Busy)?),
        None => None,
    };
    if manager.phys2page.len() == manager.phys2page.capacity()
        || manager.phys2page.contains_key(&frame)
        || !manager
            .phys2page
            .get(&old_phys)
            .is_some_and(|page| Arc::ptr_eq(page, &source))
    {
        return Err(Busy);
    }
    if let Some(reclaimer) = &reclaimer {
        if reclaimer.lru.len() == reclaimer.lru.capacity()
            || reclaimer.lru.peek(&frame).is_some()
            || !reclaimer
                .lru
                .peek(&old_phys)
                .is_some_and(|page| Arc::ptr_eq(page, &source))
        {
            return Err(Busy);
        }
    }
    #[cfg(feature = "mm_selftest")]
    if let (Some(control), Some((mm, address))) = (control, origin) {
        control.at(MigrationStage::BeforeCommit, mm, address)?;
    }
    let token = isolation.into_owned_for_commit();
    #[cfg(feature = "mm_selftest")]
    if let Some(control) = control {
        control.before_ownership_freeze();
    }
    drop(source);
    if let Some(guard) = cache_guard.as_mut() {
        drop(mem::replace(guard.page_mut(), target.take().unwrap()));
    }
    drop(
        manager
            .phys2page
            .remove(&old_phys)
            .expect("validated source registry"),
    );
    let position = reclaimer.as_mut().map(|reclaimer| {
        let (owner, position) = reclaimer
            .lru
            .take_for_migration(&old_phys)
            .expect("validated LRU source");
        drop(owner);
        position
    });
    let mut old = match token.try_into_page() {
        Ok(page) => page,
        Err(token) => {
            #[cfg(feature = "mm_selftest")]
            if let Some(control) = control {
                control.extra_owner_rejected();
            }
            let source = token.page();
            let retired_target = cache_guard
                .as_mut()
                .map(|guard| mem::replace(guard.page_mut(), source.clone()));
            assert!(manager.phys2page.insert(old_phys, source.clone()).is_none());
            if let (Some(reclaimer), Some(position)) = (reclaimer.as_mut(), position) {
                reclaimer.lru.restore_migrated(old_phys, source, position);
            }
            drop(cache_guard);
            drop(reclaimer);
            drop(manager);
            drop(pt_guards);
            revoked.rollback();
            drop(token);
            drop(retired_target);
            return Err(Busy);
        }
    };
    let target_slot = match cache_guard.as_mut() {
        Some(guard) => guard.page_mut(),
        None => target.as_mut().unwrap(),
    };
    let new = Arc::get_mut(target_slot).expect("destination has one prepared owner");
    // Actual source uniqueness, not its earlier read/write lock, is what
    // closes the last existing-Arc writer window before the single copy.
    unsafe {
        destination
            .as_ptr::<u8>()
            .copy_from_nonoverlapping(source_address.as_ptr::<u8>(), MMArch::PAGE_SIZE);
    }
    new.memory_charge = old.memory_charge.take();
    let old_inner = old.inner.get_mut();
    let new_inner = new.inner.get_mut();
    mem::swap(old_inner, new_inner);
    mem::swap(&mut old_inner.phys_addr, &mut new_inner.phys_addr);
    // Every logical state moves together; each Page retains its original
    // physical owner. The old body is now empty and owns only the old frame.
    let relocated = target_slot.clone();
    assert!(manager.phys2page.insert(frame, relocated.clone()).is_none());
    if let (Some(reclaimer), Some(position)) = (reclaimer.as_mut(), position) {
        reclaimer.lru.restore_migrated(frame, &relocated, position);
    }
    revoked.publish(frame);
    drop(cache_guard);
    drop(reclaimer);
    drop(manager);
    drop(pt_guards);
    revoked.flush();
    revoked.armed = false;
    drop(revoked);
    drop(spaces);
    drop(old);
    #[cfg(feature = "mm_selftest")]
    if let Some(control) = control {
        control.committed(old_phys, frame);
    }
    Ok(MigrationResult {
        old_phys,
        new_phys: frame,
    })
}

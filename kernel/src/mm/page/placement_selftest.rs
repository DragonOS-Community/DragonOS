//! Real consumer births and normal retirement, never a replacement allocator.
//! Global request counters are concurrent corroboration; exact placement and
//! fallback behavior is tested separately against private real Buddy arenas.
use super::*;
use crate::{
    cgroup::memory::{object::ObjectAccount, MemoryAccount, MemoryKind},
    filesystem::page_cache::PageCache,
    libs::mutex::Mutex,
    mm::allocator::{page_frame::FramePlacement, reserve::with_reclaim_reserve},
};
use core::sync::atomic::{AtomicU64, Ordering};

static TEST_LOCK: Mutex<()> = Mutex::new(());
static NEXT_ACCOUNT: AtomicU64 = AtomicU64::new(0x0265_1000_0000_0000);
type Statistics = ([u64; 2], [u64; 2]);

fn account() -> Result<Arc<MemoryAccount>, SystemError> {
    MemoryAccount::new(NEXT_ACCOUNT.fetch_add(1, Ordering::Relaxed), None)
}

fn charge(
    account: &Arc<MemoryAccount>,
    kind: MemoryKind,
    pages: u64,
) -> Result<MemoryCharge, SystemError> {
    account
        .try_charge(kind, pages)
        .map_err(|_| SystemError::ENOMEM)
}

fn statistics() -> Result<Statistics, SystemError> {
    LockedFrameAllocator::with_buddy(|buddy| buddy.placement_statistics())
        .ok_or(SystemError::EAGAIN_OR_EWOULDBLOCK)
}

fn observed(before: Statistics, physical: PhysAddr, requested: FramePlacement) -> bool {
    LockedFrameAllocator::with_buddy(|buddy| {
        let (requests, fallbacks) = buddy.placement_statistics();
        let index = requested as usize;
        // Another producer can contribute to these monotonically increasing
        // counters. Do not claim per-request attribution or forbid fallback.
        requests[index] > before.0[index]
            && (buddy.placement_of(physical) == requested || fallbacks[index] > before.1[index])
    })
    .unwrap_or(false)
}

struct Registered(Arc<Page>);
impl Drop for Registered {
    fn drop(&mut self) {
        detach_pages_from_reclaimer_batched(core::iter::once(&self.0));
        detach_pages_from_manager_batched(core::iter::once(&self.0));
    }
}

fn normal_and_cow() -> Result<bool, SystemError> {
    let account = account()?;
    let before = statistics()?;
    let source = Registered(allocate_registered_data_page(
        PageType::Normal,
        PageFlags::empty(),
        Some(charge(&account, MemoryKind::Anon, 1)?),
    )?);
    let mut ok = observed(before, source.0.phys_address(), FramePlacement::Movable)
        && unsafe { source.0.read().as_slice().iter().all(|byte| *byte == 0) };
    unsafe {
        source.0.write().as_slice_mut().fill(0x37);
    }
    let before = statistics()?;
    let copy = Registered(copy_registered_normal_page(
        &source.0,
        charge(&account, MemoryKind::Anon, 1)?,
    )?);
    ok &= source.0.phys_address() != copy.0.phys_address()
        && observed(before, copy.0.phys_address(), FramePlacement::Movable)
        && unsafe { copy.0.read().as_slice().iter().all(|byte| *byte == 0x37) };
    unsafe {
        copy.0.write().as_slice_mut()[0] = 0xa5;
    }
    ok &= unsafe { source.0.read().as_slice()[0] == 0x37 } && account.current_pages() == 2;
    let source_weak = Arc::downgrade(&source.0);
    let copy_weak = Arc::downgrade(&copy.0);
    drop(copy);
    drop(source);
    Ok(ok
        && account.current_pages() == 0
        && source_weak.upgrade().is_none()
        && copy_weak.upgrade().is_none())
}

fn cache(shmem: bool, reclaim_io: bool) -> Result<bool, SystemError> {
    let account = account()?;
    let creator = ObjectAccount::new(&account)?;
    let cache = if shmem {
        PageCache::new_shmem(None, None)
    } else {
        PageCache::new_unowned(None, None)
    };
    let before = statistics()?;
    let allocate = || {
        cache
            .manager()
            .try_commit_page_with_pinned(0, &account, &creator, |_, bytes| {
                bytes.fill(0x5a);
                Ok(bytes.len())
            })
            .map_err(|_| SystemError::ENOMEM)
    };
    let pin = if reclaim_io {
        with_reclaim_reserve(|| {
            let _noio = crate::cgroup::memory::io::MemoryIoScope::enter_noio();
            allocate()
        })?
    } else {
        allocate()?
    };
    let page = pin.page();
    let snapshot = account.snapshot();
    let ok = observed(before, page.phys_address(), FramePlacement::Movable)
        && matches!(page.read().page_type(), PageType::File(_))
        && unsafe { page.read().as_slice().iter().all(|byte| *byte == 0x5a) }
        && if shmem {
            // memory.stat's file total includes shmem; they must not be added.
            snapshot.shmem == 1 && snapshot.file == 1
        } else {
            snapshot.file == 1 && snapshot.shmem == 0
        };
    let weak = Arc::downgrade(&page);
    drop(page);
    drop(pin);
    drop(cache);
    drop(creator);
    Ok(ok && account.current_pages() == 0 && weak.upgrade().is_none())
}

fn intrinsic_single() -> Result<bool, SystemError> {
    let account = account()?;
    let before = statistics()?;
    let page = Registered(allocate_registered_data_page(
        PageType::Normal,
        PageFlags::PG_UNEVICTABLE,
        Some(charge(&account, MemoryKind::Anon, 1)?),
    )?);
    let ok = observed(before, page.0.phys_address(), FramePlacement::Unmovable)
        && page.0.read().flags().contains(PageFlags::PG_UNEVICTABLE);
    drop(page);
    Ok(ok && account.current_pages() == 0)
}

fn contiguous_intrinsic() -> Result<bool, SystemError> {
    let account = account()?;
    let before = statistics()?;
    // Three user pages require four admission pages; the production exact-run
    // path trims the unused tail and returns that charge before publication.
    let batch = allocate_charged_intrinsic_unevictable_pages_exact(
        PageFrameCount::new(3),
        MemoryKind::Anon,
        charge(&account, MemoryKind::Anon, 4)?,
    )?;
    let mut ok = observed(before, batch.start_paddr(), FramePlacement::Unmovable)
        && batch.pages().len() == 3
        && account.current_pages() == 3;
    for (index, page) in batch.pages().iter().enumerate() {
        ok &= page.phys_address() == batch.start_paddr() + index * MMArch::PAGE_SIZE
            && page.read().flags().contains(PageFlags::PG_UNEVICTABLE)
            && unsafe { page.read().as_slice().iter().all(|byte| *byte == 0) };
    }
    drop(batch);
    Ok(ok && account.current_pages() == 0)
}

fn raw_and_legacy() -> Result<bool, SystemError> {
    let before = statistics()?;
    let (physical, count) = unsafe {
        LockedFrameAllocator.allocate_request(FrameAllocationRequest::kernel(
            PageFrameCount::ONE,
            PhysAddr::new(usize::MAX),
        ))
    }
    .ok_or(SystemError::ENOMEM)?;
    let raw = RawFrameExtent {
        next: physical,
        remaining: count.data(),
    };
    let mut ok = observed(before, physical, FramePlacement::Unmovable);
    drop(raw);
    let before = statistics()?;
    let mut manager = PageManager::new();
    let page = manager.create_one_page(
        PageType::Normal,
        PageFlags::empty(),
        &mut LockedFrameAllocator,
    )?;
    ok &= observed(before, page.phys_address(), FramePlacement::Unmovable);
    let weak = Arc::downgrade(&page);
    drop(page);
    drop(manager);
    Ok(ok && weak.upgrade().is_none())
}

fn unsupported_shm() -> Result<bool, SystemError> {
    let account = account()?;
    let before = statistics()?;
    let page = Registered(allocate_registered_data_page(
        PageType::Shm,
        PageFlags::empty(),
        Some(charge(&account, MemoryKind::Shmem, 1)?),
    )?);
    let ok = observed(before, page.0.phys_address(), FramePlacement::Unmovable);
    drop(page);
    Ok(ok && account.current_pages() == 0)
}

fn migration_target() -> Result<bool, SystemError> {
    let before = statistics()?;
    // Entering a more permissive scope must not turn this explicit migration
    // target call into reclaim/OOM work. Its production API uses Kernel floor
    // and one ordinary Buddy attempt, even inside an unrelated reserve scope.
    let physical = with_reclaim_reserve(|| unsafe {
        LockedFrameAllocator::allocate_migration_target_no_reclaim()
    })
    .ok_or(SystemError::ENOMEM)?;
    let raw = RawFrameExtent {
        next: physical,
        remaining: 1,
    };
    let ok = observed(before, physical, FramePlacement::Movable);
    drop(raw);
    Ok(ok)
}

pub(crate) fn checks() -> [(&'static str, bool); 9] {
    let _serial = TEST_LOCK.lock();
    [
        ("normal_cow", normal_and_cow().unwrap_or(false)),
        ("file_cache", cache(false, false).unwrap_or(false)),
        ("tmpfs_cache", cache(true, false).unwrap_or(false)),
        ("file_reclaim_io", cache(false, true).unwrap_or(false)),
        ("intrinsic_single", intrinsic_single().unwrap_or(false)),
        (
            "contiguous_intrinsic",
            contiguous_intrinsic().unwrap_or(false),
        ),
        ("raw_legacy", raw_and_legacy().unwrap_or(false)),
        ("migration_target", migration_target().unwrap_or(false)),
        ("unsupported_shm", unsupported_shm().unwrap_or(false)),
    ]
}

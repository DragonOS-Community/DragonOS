use alloc::vec::Vec;
#[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
use alloc::{format, string::String};
use core::ptr::NonNull;
use system_error::SystemError;

use crate::arch::MMArch;
#[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
use crate::libs::mutex::Mutex;
use crate::libs::spinlock::SpinLock;
#[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
use crate::mm::allocator::buddy::deterministic_buddy_selftest;
use crate::mm::{
    allocator::page_frame::{
        allocate_page_frames, allocate_page_frames_below, deallocate_page_frames, PageFrameCount,
        PhysPageFrame,
    },
    MemoryManagementArch, PhysAddr,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DmaDirection {
    ToDevice,
    FromDevice,
    Bidirectional,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub enum DmaCachePolicy {
    Uncached,
    WriteCombined,
    Cached,
}

#[derive(Clone, Copy, Debug)]
#[allow(dead_code)]
pub struct DmaAllocOptions {
    pub direction: DmaDirection,
    pub cache_policy: DmaCachePolicy,
    pub zeroed: bool,
    pub dma_mask: Option<u64>,
    pub use_pool: bool,
}

impl Default for DmaAllocOptions {
    fn default() -> Self {
        Self {
            direction: DmaDirection::Bidirectional,
            cache_policy: DmaCachePolicy::Cached,
            zeroed: true,
            dma_mask: None,
            use_pool: true,
        }
    }
}

impl DmaAllocOptions {
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self::default()
    }
}

#[derive(Debug)]
pub struct DmaBuffer {
    paddr: usize,
    vaddr: NonNull<u8>,
    len: usize,
    page_count: PageFrameCount,
    pool_key: Option<DmaPoolKey>,
}

unsafe impl Send for DmaBuffer {}
unsafe impl Sync for DmaBuffer {}

impl DmaBuffer {
    pub fn alloc_bytes(size: usize, options: DmaAllocOptions) -> Self {
        Self::try_alloc_bytes(size, options).expect("dma alloc bytes failed")
    }

    /// Allocate a physically contiguous DMA buffer without turning memory
    /// pressure or invalid sizing into a kernel panic.
    pub fn try_alloc_bytes(size: usize, options: DmaAllocOptions) -> Result<Self, SystemError> {
        dma_allocator().try_alloc_bytes(size, options)
    }

    #[allow(dead_code)]
    pub fn alloc_pages(pages: usize, options: DmaAllocOptions) -> Self {
        Self::try_alloc_pages(pages, options).expect("dma alloc pages failed")
    }

    #[allow(dead_code)]
    pub fn try_alloc_pages(pages: usize, options: DmaAllocOptions) -> Result<Self, SystemError> {
        dma_allocator().try_alloc_pages(pages, options)
    }

    #[allow(dead_code)]
    pub fn paddr(&self) -> usize {
        self.paddr
    }

    #[allow(dead_code)]
    pub fn vaddr(&self) -> NonNull<u8> {
        self.vaddr
    }

    pub fn len(&self) -> usize {
        self.len
    }

    #[allow(dead_code)]
    pub fn page_count(&self) -> PageFrameCount {
        self.page_count
    }

    pub fn as_slice(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self.vaddr.as_ptr(), self.len) }
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(self.vaddr.as_ptr(), self.len) }
    }

    pub fn to_vec(&self) -> Vec<u8> {
        self.as_slice().to_vec()
    }
}

impl Drop for DmaBuffer {
    fn drop(&mut self) {
        let raw = DmaRawAllocation {
            paddr: PhysAddr::new(self.paddr),
            vaddr: self.vaddr,
            page_count: self.page_count,
        };
        let raw = if let Some(key) = self.pool_key {
            match dma_allocator().return_to_pool(key, raw) {
                Ok(()) => return,
                Err(raw) => raw,
            }
        } else {
            raw
        };
        unsafe { dma_dealloc_pages_raw(raw.paddr.data(), raw.vaddr, raw.page_count.data()) };
    }
}

#[derive(Debug)]
struct DmaRawAllocation {
    paddr: PhysAddr,
    vaddr: NonNull<u8>,
    page_count: PageFrameCount,
}

unsafe impl Send for DmaRawAllocation {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DmaPoolDomain {
    Dma32,
    Unrestricted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DmaPoolKey {
    pages: usize,
    domain: DmaPoolDomain,
}

struct DmaPool {
    pages: usize,
    max: usize,
    dma32_free_list: Vec<DmaRawAllocation>,
    unrestricted_free_list: Vec<DmaRawAllocation>,
}

impl DmaPool {
    fn new(pages: usize, max: usize) -> Self {
        let mut dma32_free_list = Vec::new();
        let mut unrestricted_free_list = Vec::new();
        // Pool metadata is optional. If reservation fails, put() observes the
        // zero capacity and the allocator safely falls back to the buddy path.
        let _ = dma32_free_list.try_reserve_exact(max);
        let _ = unrestricted_free_list.try_reserve_exact(max);
        Self {
            pages,
            max,
            dma32_free_list,
            unrestricted_free_list,
        }
    }

    fn take_fitting(
        free_list: &mut Vec<DmaRawAllocation>,
        mask: Option<u64>,
    ) -> Option<DmaRawAllocation> {
        let index = free_list
            .iter()
            .rposition(|alloc| allocation_fits_mask(alloc, mask))?;
        Some(free_list.swap_remove(index))
    }

    fn take(&mut self, domain: DmaPoolDomain, mask: Option<u64>) -> Option<DmaRawAllocation> {
        let free_list = match domain {
            DmaPoolDomain::Dma32 => &mut self.dma32_free_list,
            DmaPoolDomain::Unrestricted => &mut self.unrestricted_free_list,
        };
        Self::take_fitting(free_list, mask)
    }

    fn put(
        &mut self,
        domain: DmaPoolDomain,
        alloc: DmaRawAllocation,
    ) -> Result<(), DmaRawAllocation> {
        if alloc.page_count.data() != self.pages {
            return Err(alloc);
        }
        let Some(end) = allocation_end(&alloc) else {
            return Err(alloc);
        };
        let free_list = match domain {
            DmaPoolDomain::Dma32 if usize::BITS > 32 && end > u32::MAX as usize => {
                return Err(alloc)
            }
            DmaPoolDomain::Dma32 => &mut self.dma32_free_list,
            DmaPoolDomain::Unrestricted => &mut self.unrestricted_free_list,
        };
        if free_list.len() >= self.max || free_list.len() == free_list.capacity() {
            return Err(alloc);
        }
        debug_assert!(free_list.len() < free_list.capacity());
        free_list.push(alloc);
        Ok(())
    }
}

pub struct DmaAllocator {
    pools: Vec<SpinLock<DmaPool>>,
}

impl DmaAllocator {
    fn new() -> Self {
        let mut pools = Vec::new();
        for pages in DMA_POOL_CLASSES {
            pools.push(SpinLock::new(DmaPool::new(*pages, DMA_POOL_MAX_PER_DOMAIN)));
        }
        Self { pools }
    }

    pub fn try_alloc_bytes(
        &self,
        size: usize,
        options: DmaAllocOptions,
    ) -> Result<DmaBuffer, SystemError> {
        let page_count = page_count_from_bytes(size).ok_or(SystemError::ENOMEM)?;
        self.try_alloc_with_pages(page_count, size, options)
    }

    #[allow(dead_code)]
    pub fn try_alloc_pages(
        &self,
        pages: usize,
        options: DmaAllocOptions,
    ) -> Result<DmaBuffer, SystemError> {
        let page_count = page_count_from_pages(pages).ok_or(SystemError::ENOMEM)?;
        let size = pages
            .checked_mul(MMArch::PAGE_SIZE)
            .ok_or(SystemError::ENOMEM)?;
        self.try_alloc_with_pages(page_count, size, options)
    }

    fn try_alloc_with_pages(
        &self,
        page_count: PageFrameCount,
        len: usize,
        options: DmaAllocOptions,
    ) -> Result<DmaBuffer, SystemError> {
        // DragonOS currently supports coherent DMA mappings only. Keep RAM in
        // its normal write-back direct mapping, as Linux does for coherent
        // devices; changing PAT/cacheability requires architecture-specific
        // cache maintenance and cannot be emulated by rewriting PTE flags.
        validate_cache_policy(options.cache_policy)?;
        let pool_key = Self::pool_key_for(page_count.data(), options.use_pool, options.dma_mask);
        let raw = self.try_alloc_from_pool_or_raw(page_count, pool_key, &options)?;
        Ok(DmaBuffer {
            paddr: raw.paddr.data(),
            vaddr: raw.vaddr,
            len,
            page_count: raw.page_count,
            pool_key,
        })
    }

    fn try_alloc_from_pool_or_raw(
        &self,
        page_count: PageFrameCount,
        pool_key: Option<DmaPoolKey>,
        options: &DmaAllocOptions,
    ) -> Result<DmaRawAllocation, SystemError> {
        if let Some(key) = pool_key {
            if let Some(raw) = self.take_from_pool(key, options.dma_mask) {
                if options.zeroed {
                    self.zero_raw(&raw);
                }
                return Ok(raw);
            }
        }
        match self.try_alloc_raw(page_count, options) {
            Ok(raw) => Ok(raw),
            Err(SystemError::ENOMEM)
                if pool_key.is_some_and(|key| key.domain == DmaPoolDomain::Dma32) =>
            {
                // Preserve the normal hot-path partition between wide and
                // DMA32 users. Only after the constrained buddy allocation is
                // exhausted may a DMA32 request reclaim a compatible low
                // buffer cached by a wide request. The caller's Dma32 key then
                // makes this a one-way migration on Drop.
                let key = DmaPoolKey {
                    pages: page_count.data(),
                    domain: DmaPoolDomain::Unrestricted,
                };
                let raw = self
                    .take_from_pool(key, options.dma_mask)
                    .ok_or(SystemError::ENOMEM)?;
                if options.zeroed {
                    self.zero_raw(&raw);
                }
                Ok(raw)
            }
            Err(err) => Err(err),
        }
    }

    fn try_alloc_raw(
        &self,
        page_count: PageFrameCount,
        options: &DmaAllocOptions,
    ) -> Result<DmaRawAllocation, SystemError> {
        validate_cache_policy(options.cache_policy)?;
        let allocation = if let Some(mask) = options.dma_mask {
            let max = usize::try_from(mask).unwrap_or(usize::MAX);
            unsafe { allocate_page_frames_below(page_count, PhysAddr::new(max)) }
        } else {
            unsafe { allocate_page_frames(page_count) }
        };
        let (paddr, count) = allocation.ok_or(SystemError::ENOMEM)?;
        let virt = match unsafe { MMArch::phys_2_virt(paddr) } {
            Some(virt) => virt,
            None => {
                unsafe {
                    deallocate_page_frames(PhysPageFrame::new(paddr), count);
                }
                return Err(SystemError::ENOMEM);
            }
        };
        if options.zeroed {
            unsafe {
                core::ptr::write_bytes(virt.data() as *mut u8, 0, count.data() * MMArch::PAGE_SIZE);
            }
        }
        Ok(DmaRawAllocation {
            paddr,
            vaddr: NonNull::new(virt.data() as *mut u8).unwrap(),
            page_count: count,
        })
    }

    fn zero_raw(&self, alloc: &DmaRawAllocation) {
        unsafe {
            core::ptr::write_bytes(
                alloc.vaddr.as_ptr(),
                0,
                alloc.page_count.data() * MMArch::PAGE_SIZE,
            );
        }
    }

    fn pool_key_for(pages: usize, use_pool: bool, mask: Option<u64>) -> Option<DmaPoolKey> {
        if !use_pool {
            return None;
        }
        for class in DMA_POOL_CLASSES {
            if pages == *class {
                let domain = if usize::BITS > 32 && mask.is_some_and(|mask| mask <= u32::MAX as u64)
                {
                    DmaPoolDomain::Dma32
                } else {
                    DmaPoolDomain::Unrestricted
                };
                return Some(DmaPoolKey {
                    pages: *class,
                    domain,
                });
            }
        }
        None
    }

    fn pool_for_pages(&self, pages: usize) -> Option<&SpinLock<DmaPool>> {
        let index = DMA_POOL_CLASSES.iter().position(|class| *class == pages)?;
        self.pools.get(index)
    }

    fn take_from_pool(&self, key: DmaPoolKey, mask: Option<u64>) -> Option<DmaRawAllocation> {
        self.pool_for_pages(key.pages)?
            .lock_irqsave()
            .take(key.domain, mask)
    }

    fn return_to_pool(
        &self,
        key: DmaPoolKey,
        alloc: DmaRawAllocation,
    ) -> Result<(), DmaRawAllocation> {
        let Some(pool) = self.pool_for_pages(key.pages) else {
            return Err(alloc);
        };
        pool.lock_irqsave().put(key.domain, alloc)
    }
}

fn allocation_end(allocation: &DmaRawAllocation) -> Option<usize> {
    let bytes = allocation
        .page_count
        .data()
        .checked_mul(MMArch::PAGE_SIZE)?;
    allocation.paddr.data().checked_add(bytes.checked_sub(1)?)
}

fn allocation_fits_mask(allocation: &DmaRawAllocation, mask: Option<u64>) -> bool {
    let Some(end) = allocation_end(allocation).and_then(|end| u64::try_from(end).ok()) else {
        return false;
    };
    mask.is_none_or(|mask| end <= mask)
}

pub fn dma_alloc_pages_raw(pages: usize, mut options: DmaAllocOptions) -> (usize, NonNull<u8>) {
    options.use_pool = false;
    let page_count = page_count_from_pages(pages).expect("invalid dma page count");
    let raw = dma_allocator()
        .try_alloc_raw(page_count, &options)
        .expect("dma alloc pages failed");
    (raw.paddr.data(), raw.vaddr)
}

pub unsafe fn dma_dealloc_pages_raw(paddr: usize, vaddr: NonNull<u8>, pages: usize) -> i32 {
    let page_count = page_count_from_pages(pages).expect("invalid dma deallocation page count");
    debug_assert_eq!(
        unsafe { MMArch::phys_2_virt(PhysAddr::new(paddr)) }.map(|addr| addr.data()),
        Some(vaddr.as_ptr() as usize)
    );
    unsafe {
        deallocate_page_frames(PhysPageFrame::new(PhysAddr::new(paddr)), page_count);
    }
    0
}

fn validate_cache_policy(cache_policy: DmaCachePolicy) -> Result<(), SystemError> {
    if cache_policy == DmaCachePolicy::Cached {
        Ok(())
    } else {
        Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
    }
}

fn page_count_from_pages(pages: usize) -> Option<PageFrameCount> {
    let pages = pages.max(1);
    Some(PageFrameCount::new(pages.checked_next_power_of_two()?))
}

fn page_count_from_bytes(size: usize) -> Option<PageFrameCount> {
    let pages = size.div_ceil(MMArch::PAGE_SIZE).max(1);
    page_count_from_pages(pages)
}

/// Each size class keeps independent bounded caches for DMA32-constrained and
/// wider requests. On 64-bit systems the combined worst-case cache is 15.5 MiB.
const DMA_POOL_MAX_PER_DOMAIN: usize = 64;
const DMA_POOL_CLASSES: &[usize] = &[1, 2, 4, 8, 16];

lazy_static! {
    static ref DMA_ALLOCATOR: DmaAllocator = DmaAllocator::new();
}

#[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
lazy_static! {
    static ref DMA_SELFTEST_LOCK: Mutex<()> = Mutex::new(());
}

fn dma_allocator() -> &'static DmaAllocator {
    &DMA_ALLOCATOR
}

#[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
fn release_raw(raw: DmaRawAllocation) {
    unsafe {
        deallocate_page_frames(PhysPageFrame::new(raw.paddr), raw.page_count);
    }
}

#[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
fn raw_fits_mask(raw: &DmaRawAllocation, mask: u64) -> bool {
    allocation_fits_mask(raw, Some(mask))
}

#[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
fn selftest_bounded_orders() -> bool {
    let options = DmaAllocOptions {
        zeroed: false,
        dma_mask: Some(u32::MAX as u64),
        use_pool: false,
        ..Default::default()
    };

    for pages in [1, 2, 4, 8, 16] {
        let Ok(raw) = dma_allocator().try_alloc_raw(PageFrameCount::new(pages), &options) else {
            return false;
        };
        let ok = raw.page_count.data() == pages && raw_fits_mask(&raw, u32::MAX as u64);
        release_raw(raw);
        if !ok {
            return false;
        }
    }
    true
}

#[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
fn selftest_raw(paddr: usize, pages: usize) -> DmaRawAllocation {
    DmaRawAllocation {
        paddr: PhysAddr::new(paddr),
        vaddr: NonNull::dangling(),
        page_count: PageFrameCount::new(pages),
    }
}

#[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
fn selftest_pool_mask_separation() -> bool {
    let mut pool = DmaPool::new(1, DMA_POOL_MAX_PER_DOMAIN);

    // A narrow mask must skip, but not discard, an incompatible DMA32 entry.
    if pool
        .put(DmaPoolDomain::Dma32, selftest_raw(0x1000, 1))
        .is_err()
        || pool
            .put(DmaPoolDomain::Dma32, selftest_raw(0x2000, 1))
            .is_err()
    {
        return false;
    }
    let narrow = pool.take(DmaPoolDomain::Dma32, Some(0x1fff));
    let narrow_preserved = pool.dma32_free_list.len() == 1;
    let remaining_dma32 = pool.take(DmaPoolDomain::Dma32, Some(u32::MAX as u64));

    // A finite mask above 32 bits still needs exact range filtering inside
    // the unrestricted domain.
    let forty_bit_mask = (1u64 << 40) - 1;
    if pool
        .put(DmaPoolDomain::Unrestricted, selftest_raw(1usize << 32, 1))
        .is_err()
        || pool
            .put(DmaPoolDomain::Unrestricted, selftest_raw(1usize << 40, 1))
            .is_err()
    {
        return false;
    }
    let forty_bit = pool.take(DmaPoolDomain::Unrestricted, Some(forty_bit_mask));
    let forty_bit_preserved = pool.unrestricted_free_list.len() == 1;
    let remaining_unrestricted = pool.take(DmaPoolDomain::Unrestricted, None);

    // DMA32 admission uses the inclusive end of the whole allocation, not
    // just its first page. Arithmetic overflow is never admitted to a pool.
    let mut two_page_pool = DmaPool::new(2, DMA_POOL_MAX_PER_DOMAIN);
    let crosses_dma32 = u32::MAX as usize - MMArch::PAGE_SIZE + 1;
    let crossing_rejected_by_dma32 = two_page_pool
        .put(DmaPoolDomain::Dma32, selftest_raw(crosses_dma32, 2))
        .is_err();
    let crossing_put =
        two_page_pool.put(DmaPoolDomain::Unrestricted, selftest_raw(crosses_dma32, 2));
    let crossing_available_unrestricted = two_page_pool
        .take(DmaPoolDomain::Unrestricted, None)
        .is_some();
    let overflow_rejected = two_page_pool
        .put(
            DmaPoolDomain::Unrestricted,
            selftest_raw(usize::MAX - MMArch::PAGE_SIZE + 1, 2),
        )
        .is_err();

    narrow.is_some_and(|raw| raw.paddr.data() == 0x1000)
        && narrow_preserved
        && remaining_dma32.is_some_and(|raw| raw.paddr.data() == 0x2000)
        && forty_bit.is_some_and(|raw| raw.paddr.data() == 1usize << 32)
        && forty_bit_preserved
        && remaining_unrestricted.is_some_and(|raw| raw.paddr.data() == 1usize << 40)
        && crossing_put.is_ok()
        && crossing_rejected_by_dma32
        && crossing_available_unrestricted
        && overflow_rejected
}

#[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
fn selftest_pool_domain_separation() -> bool {
    let key_selection_ok = DmaAllocator::pool_key_for(1, true, Some(u32::MAX as u64))
        == Some(DmaPoolKey {
            pages: 1,
            domain: DmaPoolDomain::Dma32,
        })
        && DmaAllocator::pool_key_for(1, true, Some((1u64 << 40) - 1))
            == Some(DmaPoolKey {
                pages: 1,
                domain: DmaPoolDomain::Unrestricted,
            })
        && DmaAllocator::pool_key_for(1, true, None)
            == Some(DmaPoolKey {
                pages: 1,
                domain: DmaPoolDomain::Unrestricted,
            })
        && DmaAllocator::pool_key_for(1, true, Some(u64::MAX))
            == Some(DmaPoolKey {
                pages: 1,
                domain: DmaPoolDomain::Unrestricted,
            })
        && DmaAllocator::pool_key_for(3, true, None).is_none()
        && DmaAllocator::pool_key_for(1, false, None).is_none();
    if !key_selection_ok {
        return false;
    }

    let mut pool = DmaPool::new(1, 1);
    if pool
        .put(DmaPoolDomain::Dma32, selftest_raw(0x1000, 1))
        .is_err()
        || pool
            .put(DmaPoolDomain::Unrestricted, selftest_raw(1usize << 32, 1))
            .is_err()
    {
        return false;
    }

    let low = pool.take(DmaPoolDomain::Dma32, Some(u32::MAX as u64));
    let high_untouched = pool.unrestricted_free_list.len() == 1;
    let high = pool.take(DmaPoolDomain::Unrestricted, Some(u64::MAX));

    // A wide request may receive low physical memory on a small system. It
    // remains reusable in its logical domain without becoming visible to a
    // DMA32 request.
    if pool
        .put(DmaPoolDomain::Unrestricted, selftest_raw(0x2000, 1))
        .is_err()
    {
        return false;
    }
    let dma32_did_not_cross = pool
        .take(DmaPoolDomain::Dma32, Some(u32::MAX as u64))
        .is_none();
    let low_wide_reused = pool.take(DmaPoolDomain::Unrestricted, None);

    let per_domain_capacity = pool
        .put(DmaPoolDomain::Dma32, selftest_raw(0x3000, 1))
        .is_ok()
        && pool
            .put(DmaPoolDomain::Dma32, selftest_raw(0x4000, 1))
            .is_err()
        && pool
            .put(DmaPoolDomain::Unrestricted, selftest_raw(0x5000, 1))
            .is_ok()
        && pool
            .put(DmaPoolDomain::Unrestricted, selftest_raw(0x6000, 1))
            .is_err();

    // Under DMA32 pressure, a compatible low allocation may migrate out of
    // the wide-request cache without discarding an incompatible high entry.
    let mut migration_pool = DmaPool::new(1, DMA_POOL_MAX_PER_DOMAIN);
    let migration_setup = migration_pool
        .put(DmaPoolDomain::Unrestricted, selftest_raw(0x7000, 1))
        .is_ok()
        && migration_pool
            .put(DmaPoolDomain::Unrestricted, selftest_raw(1usize << 32, 1))
            .is_ok();
    let migrated = migration_pool.take(DmaPoolDomain::Unrestricted, Some(u32::MAX as u64));
    let incompatible_preserved = migration_pool.unrestricted_free_list.len() == 1;
    let migration_returned = migrated
        .map(|raw| migration_pool.put(DmaPoolDomain::Dma32, raw).is_ok())
        .unwrap_or(false);
    let migrated_reused = migration_pool
        .take(DmaPoolDomain::Dma32, Some(u32::MAX as u64))
        .is_some_and(|raw| raw.paddr.data() == 0x7000);

    low.is_some_and(|raw| raw.paddr.data() == 0x1000)
        && high_untouched
        && high.is_some_and(|raw| raw.paddr.data() == 1usize << 32)
        && dma32_did_not_cross
        && low_wide_reused.is_some_and(|raw| raw.paddr.data() == 0x2000)
        && per_domain_capacity
        && migration_setup
        && incompatible_preserved
        && migration_returned
        && migrated_reused
}

/// Run allocator checks against the live buddy allocator.  Every successful
/// allocation is released before the function returns, so reading the report
/// does not reserve memory or grow the normal DMA pools.
#[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
pub(crate) fn dma_allocator_selftest_report() -> String {
    let _guard = DMA_SELFTEST_LOCK.lock();
    let (
        bounded_candidate_selection,
        split_free_merge,
        fragmented_arena,
        dma32_zone,
        metadata_reuse,
    ) = deterministic_buddy_selftest();
    let cases = [
        ("bounded_orders", selftest_bounded_orders()),
        ("bounded_candidate_selection", bounded_candidate_selection),
        ("split_free_merge", split_free_merge),
        ("fragmented_arena", fragmented_arena),
        ("dma32_zone", dma32_zone),
        ("metadata_reuse", metadata_reuse),
        ("pool_mask_separation", selftest_pool_mask_separation()),
        ("pool_domain_separation", selftest_pool_domain_separation()),
    ];
    let failed = cases.iter().filter(|(_, passed)| !passed).count();
    let mut report = format!("status={}\n", if failed == 0 { "ok" } else { "fail" });
    for (name, passed) in cases {
        report.push_str(&format!("{name}={}\n", if passed { "ok" } else { "fail" }));
    }
    report.push_str(&format!(
        "summary_pass={}\nsummary_fail={failed}\n",
        cases.len() - failed
    ));
    report
}

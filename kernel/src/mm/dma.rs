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
    pool_pages: Option<usize>,
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
        if self.pool_pages.is_some()
            && dma_allocator().return_to_pool(DmaRawAllocation {
                paddr: PhysAddr::new(self.paddr),
                vaddr: self.vaddr,
                page_count: self.page_count,
            })
        {
            return;
        }
        unsafe { dma_dealloc_pages_raw(self.paddr, self.vaddr, self.page_count.data()) };
    }
}

#[derive(Debug)]
struct DmaRawAllocation {
    paddr: PhysAddr,
    vaddr: NonNull<u8>,
    page_count: PageFrameCount,
}

unsafe impl Send for DmaRawAllocation {}

struct DmaPool {
    pages: usize,
    max: usize,
    free_list: Vec<DmaRawAllocation>,
}

impl DmaPool {
    fn new(pages: usize, max: usize) -> Self {
        Self {
            pages,
            max,
            free_list: Vec::new(),
        }
    }

    fn take(&mut self) -> Option<DmaRawAllocation> {
        self.free_list.pop()
    }

    fn put(&mut self, alloc: DmaRawAllocation) -> bool {
        if self.free_list.len() >= self.max {
            return false;
        }
        self.free_list.push(alloc);
        true
    }
}

pub struct DmaAllocator {
    pools: Vec<SpinLock<DmaPool>>,
}

impl DmaAllocator {
    fn new() -> Self {
        let mut pools = Vec::new();
        for pages in DMA_POOL_CLASSES {
            pools.push(SpinLock::new(DmaPool::new(*pages, DMA_POOL_MAX_PER_CLASS)));
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
        let pool_pages = self.pool_pages_for(page_count.data(), options.use_pool);
        let raw = self.try_alloc_from_pool_or_raw(page_count, pool_pages, &options)?;
        Ok(DmaBuffer {
            paddr: raw.paddr.data(),
            vaddr: raw.vaddr,
            len,
            page_count: raw.page_count,
            pool_pages,
        })
    }

    fn try_alloc_from_pool_or_raw(
        &self,
        page_count: PageFrameCount,
        pool_pages: Option<usize>,
        options: &DmaAllocOptions,
    ) -> Result<DmaRawAllocation, SystemError> {
        if let Some(pages) = pool_pages {
            while let Some(raw) = self.take_from_pool(pages) {
                if allocation_fits_mask(&raw, options.dma_mask) {
                    if options.zeroed {
                        self.zero_raw(&raw);
                    }
                    return Ok(raw);
                }

                // Keep scanning the bounded pool: a compatible low-address
                // entry may sit below this one in the LIFO free list. Returning
                // incompatible entries to the buddy allocator also prevents
                // them from repeatedly blocking future bounded callers.
                unsafe {
                    deallocate_page_frames(PhysPageFrame::new(raw.paddr), raw.page_count);
                }
            }
        }
        self.try_alloc_raw(page_count, options)
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

    fn pool_pages_for(&self, pages: usize, use_pool: bool) -> Option<usize> {
        if !use_pool {
            return None;
        }
        for class in DMA_POOL_CLASSES {
            if pages == *class {
                return Some(*class);
            }
        }
        None
    }

    fn take_from_pool(&self, pages: usize) -> Option<DmaRawAllocation> {
        for pool in &self.pools {
            let mut guard = pool.lock_irqsave();
            if guard.pages == pages {
                return guard.take();
            }
        }
        None
    }

    fn return_to_pool(&self, alloc: DmaRawAllocation) -> bool {
        for pool in &self.pools {
            let mut guard = pool.lock_irqsave();
            if guard.pages == alloc.page_count.data() {
                return guard.put(alloc);
            }
        }
        false
    }
}

fn allocation_fits_mask(allocation: &DmaRawAllocation, mask: Option<u64>) -> bool {
    mask.is_none_or(|mask| {
        allocation
            .paddr
            .data()
            .checked_add(allocation.page_count.bytes().saturating_sub(1))
            .is_some_and(|end| end as u64 <= mask)
    })
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

const DMA_POOL_MAX_PER_CLASS: usize = 64;
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
fn selftest_pool_mask_separation() -> bool {
    let allocator = DmaAllocator::new();
    let unbounded = DmaAllocOptions {
        zeroed: false,
        use_pool: false,
        ..Default::default()
    };
    let Ok(first) = allocator.try_alloc_raw(PageFrameCount::ONE, &unbounded) else {
        return false;
    };
    let Ok(second) = allocator.try_alloc_raw(PageFrameCount::ONE, &unbounded) else {
        release_raw(first);
        return false;
    };
    let (lower, higher) = if first.paddr < second.paddr {
        (first, second)
    } else {
        (second, first)
    };
    let Some(mask) = lower
        .paddr
        .data()
        .checked_add(lower.page_count.bytes().saturating_sub(1))
        .map(|end| end as u64)
    else {
        release_raw(lower);
        release_raw(higher);
        return false;
    };
    let lower_copy = DmaRawAllocation {
        paddr: lower.paddr,
        vaddr: lower.vaddr,
        page_count: lower.page_count,
    };
    if !allocator.return_to_pool(lower) {
        release_raw(lower_copy);
        release_raw(higher);
        return false;
    }
    let higher_copy = DmaRawAllocation {
        paddr: higher.paddr,
        vaddr: higher.vaddr,
        page_count: higher.page_count,
    };
    if !allocator.return_to_pool(higher) {
        if let Some(pooled_lower) = allocator.take_from_pool(1) {
            release_raw(pooled_lower);
        }
        release_raw(higher_copy);
        return false;
    }

    let bounded = DmaAllocOptions {
        dma_mask: Some(mask),
        use_pool: true,
        ..unbounded
    };
    match allocator.try_alloc_from_pool_or_raw(PageFrameCount::ONE, Some(1), &bounded) {
        Ok(raw) => {
            let ok = raw.paddr == lower_copy.paddr && raw_fits_mask(&raw, mask);
            release_raw(raw);
            ok
        }
        Err(_) => {
            while let Some(raw) = allocator.take_from_pool(1) {
                release_raw(raw);
            }
            false
        }
    }
}

/// Run allocator checks against the live buddy allocator.  Every successful
/// allocation is released before the function returns, so reading the report
/// does not reserve memory or grow the normal DMA pools.
#[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
pub(crate) fn dma_allocator_selftest_report() -> String {
    let _guard = DMA_SELFTEST_LOCK.lock();
    let (bounded_candidate_selection, split_free_merge, fragmented_arena) =
        deterministic_buddy_selftest();
    let cases = [
        ("bounded_orders", selftest_bounded_orders()),
        ("bounded_candidate_selection", bounded_candidate_selection),
        ("split_free_merge", split_free_merge),
        ("fragmented_arena", fragmented_arena),
        ("pool_mask_separation", selftest_pool_mask_separation()),
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

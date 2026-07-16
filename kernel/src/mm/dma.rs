use alloc::vec::Vec;
use core::ptr::NonNull;
use system_error::SystemError;

use crate::arch::mm::kernel_page_flags;
use crate::arch::MMArch;
use crate::libs::spinlock::SpinLock;
use crate::mm::kernel_mapper::KernelMapper;
use crate::mm::page::EntryFlags;
use crate::mm::{
    allocator::page_frame::{
        allocate_page_frames, deallocate_page_frames, PageFrameCount, PhysPageFrame,
    },
    MemoryManagementArch, PhysAddr, VirtAddr,
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
            cache_policy: DmaCachePolicy::Uncached,
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
    #[allow(dead_code)]
    cache_policy: DmaCachePolicy,
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
        unsafe {
            dma_dealloc_pages_raw(self.paddr, self.vaddr, self.page_count.data());
        }
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
        let pool_pages = self.pool_pages_for(page_count.data(), options.use_pool);
        let raw = if let Some(pages) = pool_pages {
            if let Some(raw) = self.take_from_pool(pages) {
                if options.zeroed {
                    self.zero_raw(&raw);
                }
                raw
            } else {
                self.try_alloc_raw(page_count, &options)?
            }
        } else {
            self.try_alloc_raw(page_count, &options)?
        };
        Ok(DmaBuffer {
            paddr: raw.paddr.data(),
            vaddr: raw.vaddr,
            len,
            page_count: raw.page_count,
            cache_policy: options.cache_policy,
            pool_pages,
        })
    }

    fn try_alloc_raw(
        &self,
        page_count: PageFrameCount,
        options: &DmaAllocOptions,
    ) -> Result<DmaRawAllocation, SystemError> {
        let (paddr, count) =
            unsafe { allocate_page_frames(page_count) }.ok_or(SystemError::ENOMEM)?;
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
        let dma_flags: EntryFlags<MMArch> = match options.cache_policy {
            DmaCachePolicy::Uncached => EntryFlags::mmio_flags(),
            DmaCachePolicy::WriteCombined => EntryFlags::mmio_flags(),
            DmaCachePolicy::Cached => EntryFlags::mmio_flags(),
        };
        let mut kernel_mapper = KernelMapper::lock();
        let Some(kernel_mapper) = kernel_mapper.as_mut() else {
            unsafe {
                deallocate_page_frames(PhysPageFrame::new(paddr), count);
            }
            return Err(SystemError::ENOMEM);
        };
        let Some(flusher) = (unsafe { kernel_mapper.remap(virt, dma_flags) }) else {
            unsafe {
                deallocate_page_frames(PhysPageFrame::new(paddr), count);
            }
            return Err(SystemError::ENOMEM);
        };
        flusher.flush();
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
    let vaddr = VirtAddr::new(vaddr.as_ptr() as usize);
    let mut kernel_mapper = KernelMapper::lock();
    let kernel_mapper = kernel_mapper.as_mut().unwrap();
    let flusher = kernel_mapper
        .remap(vaddr, kernel_page_flags(vaddr))
        .expect("dma remap failed");
    flusher.flush();
    unsafe {
        deallocate_page_frames(PhysPageFrame::new(PhysAddr::new(paddr)), page_count);
    }
    0
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

fn dma_allocator() -> &'static DmaAllocator {
    &DMA_ALLOCATOR
}

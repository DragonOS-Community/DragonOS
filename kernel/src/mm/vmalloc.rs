//! Explicit, fallible, virtually contiguous ordinary RAM ownership.
//!
//! Allocation and destruction require sleepable task context, with interrupts
//! enabled and no MM/allocator resource locks held. This is not a GlobalAlloc
//! backend or a DMA API. Upper kernel tables are published before user roots
//! are created; leaf-table ownership is shared by allocations in the same slot.

use alloc::sync::Arc;
use core::alloc::Layout;
use system_error::SystemError;

#[cfg(feature = "mm_selftest")]
use crate::cgroup::memory::object_for_task;

use crate::{
    arch::{mm::LockedFrameAllocator, CurrentIrqArch, MMArch},
    cgroup::memory::{
        accounting::KernelByteKind,
        object::{ObjectAccount, ObjectCharge},
        owned::OwnedBox,
    },
    exception::{in_interrupt, InterruptArch},
    libs::spinlock::SpinLock,
    mm::{
        allocation::MemoryAllocationError,
        allocator::page_frame::{FrameAllocator, PageFrameCount},
        kernel_mapper::KernelMapper,
        page::{CreatedPageTable, EntryFlags},
        tlb::flush_tlb_kernel_range,
        MemoryManagementArch, PhysAddr, VirtAddr,
    },
    process::ProcessManager,
};

#[cfg(target_arch = "x86_64")]
const BASE: usize = 0xffff_a200_0000_0000;
#[cfg(target_arch = "riscv64")]
const BASE: usize = 0xffff_ffff_0000_0000;
const SIZE: usize = 1 << 30;
const PAGES: usize = SIZE / MMArch::PAGE_SIZE;
const TABLE_PAGES: usize = MMArch::PAGE_ENTRY_NUM;
const SLOTS: usize = PAGES / TABLE_PAGES;
// Isolate selftests at leaf-table boundaries without reserving physical RAM.
#[cfg(feature = "mm_selftest")]
const SELFTEST_PAGES: usize = 4 * TABLE_PAGES;
#[cfg(feature = "mm_selftest")]
const SELFTEST_START: usize = PAGES - SELFTEST_PAGES;
const CHUNK: usize = 64;

pub fn contains(address: VirtAddr) -> bool {
    address.data().wrapping_sub(BASE) < SIZE
}

#[derive(Debug)]
struct LeafTable {
    record: CreatedPageTable,
    frame: PhysAddr,
    // Released only after returning the physical table frame.
    _charge: ObjectCharge,
}

impl Drop for LeafTable {
    fn drop(&mut self) {
        unsafe { LockedFrameAllocator.free_one(self.frame) };
    }
}

struct Pool {
    used: [u64; PAGES / 64],
    tables: [Option<LeafTable>; SLOTS],
    ready: bool,
}

static POOL: SpinLock<Pool> = SpinLock::new(Pool {
    used: [0; PAGES / 64],
    tables: [const { None }; SLOTS],
    ready: false,
});

impl Pool {
    fn reserve(
        &mut self,
        pages: usize,
        align: usize,
        #[cfg(feature = "mm_selftest")] isolated: bool,
    ) -> Option<usize> {
        #[cfg(feature = "mm_selftest")]
        let (mut start, end) = if isolated {
            (SELFTEST_START, PAGES)
        } else {
            (0, SELFTEST_START)
        };
        #[cfg(not(feature = "mm_selftest"))]
        let (mut start, end) = (0, PAGES);
        if !self.ready || pages > end - start {
            return None;
        }
        while start <= end - pages {
            let address = BASE.checked_add(start * MMArch::PAGE_SIZE)?;
            let aligned = address.checked_add(align - 1)? & !(align - 1);
            start = (aligned - BASE) / MMArch::PAGE_SIZE;
            if start > end - pages {
                return None;
            }
            let mut occupied = None;
            for word in start / 64..(start + pages).div_ceil(64) {
                let mask = Self::mask(start, start + pages, word);
                let bits = self.used[word] & mask;
                if bits != 0 {
                    occupied = Some(word * 64 + 63 - bits.leading_zeros() as usize);
                    break;
                }
            }
            if let Some(page) = occupied {
                start = page + 1;
                continue;
            }
            self.set_range(start, pages, true);
            return Some(start);
        }
        None
    }

    fn mask(start: usize, end: usize, word: usize) -> u64 {
        let low = start.saturating_sub(word * 64).min(64);
        let high = (end - word * 64).min(64);
        (u64::MAX << low) & (u64::MAX >> (64 - high))
    }

    fn set_range(&mut self, start: usize, pages: usize, used: bool) {
        for word in start / 64..(start + pages).div_ceil(64) {
            let mask = Self::mask(start, start + pages, word);
            if used {
                self.used[word] |= mask;
            } else {
                self.used[word] &= !mask;
            }
        }
    }
}

/// Called once, before any process copies the kernel root.
/// Upper tables are fixed system overhead (two pages on x86, one on Sv39).
pub(crate) unsafe fn init() {
    let blocks = crate::mm::memblock::mem_block_manager();
    for index in 0..blocks.total_initial_memory_regions() {
        let area = blocks.get_initial_memory_region(index).unwrap();
        let start = area
            .base
            .data()
            .checked_add(MMArch::PHYS_OFFSET)
            .expect("RAM direct map overflow");
        let end = start
            .checked_add(area.size)
            .expect("RAM direct map overflow");
        assert!(
            end <= BASE || start >= BASE + SIZE,
            "RAM overlaps reserved vmalloc VA"
        );
    }
    let _irq = CurrentIrqArch::save_and_disable_irq();
    let mut pool = POOL.lock_irqsave();
    let mut mapper = KernelMapper::lock();
    mapper
        .as_mut()
        .expect("recursive vmalloc init")
        .prepare_kernel_subtree(VirtAddr::new(BASE), 1)
        .expect("cannot prepare vmalloc shared subtree");
    pool.ready = true;
}

fn frame_zeroed() -> Result<PhysAddr, SystemError> {
    let (frame, count) = unsafe {
        LockedFrameAllocator.allocate_below(PageFrameCount::ONE, PhysAddr::new(usize::MAX))
    }
    .ok_or(SystemError::ENOMEM)?;
    assert_eq!(count, PageFrameCount::ONE);
    let address = unsafe { MMArch::phys_2_virt(frame) }.expect("allocated RAM outside direct map");
    unsafe { core::ptr::write_bytes(address.as_ptr::<u8>(), 0, MMArch::PAGE_SIZE) };
    Ok(frame)
}

#[derive(Debug)]
struct Chunk {
    next: Option<OwnedBox<Chunk>>,
    offset: usize,
    frames: [PhysAddr; CHUNK],
    acquired: usize,
    mapped: usize,
    // A contiguous CHUNK-page range can intersect at most two leaf tables.
    retired: [Option<LeafTable>; 2],
}

/// Owns zeroed RW/NX/WB RAM with stable contiguous VA and arbitrary PFNs.
/// Both construction and Drop must run in sleepable task context without
/// MM/allocator locks. No automatic physical-page migration is implied.
pub(crate) struct VmAllocation {
    start: usize,
    pages: usize,
    len: usize,
    chunks: Option<OwnedBox<Chunk>>,
    _charge: ObjectCharge,
}

pub(super) fn task_context() -> bool {
    CurrentIrqArch::is_irq_enabled()
        && !in_interrupt()
        && ProcessManager::current_pcb().preempt_count() == 0
}

impl VmAllocation {
    /// Whether this platform can provide the required writable, non-executable
    /// mappings. This does not promise address space, memory, or task context.
    pub(super) fn is_supported() -> bool {
        #[cfg(target_arch = "x86_64")]
        {
            !crate::arch::mm::X86_64MMArch::is_xd_reserved()
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            true
        }
    }

    #[cfg(feature = "mm_selftest")]
    pub(crate) fn try_new_zeroed(layout: Layout) -> Result<Self, MemoryAllocationError> {
        if !task_context() {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK.into());
        }
        Self::allocate_with(
            layout,
            &mut AllocationTest::isolated(None),
            &mut frame_zeroed,
        )
    }

    pub(super) fn try_new_zeroed_in(
        layout: Layout,
        owner: &Arc<ObjectAccount>,
    ) -> Result<Self, MemoryAllocationError> {
        Self::allocate_with_owner(
            layout,
            owner,
            #[cfg(feature = "mm_selftest")]
            &mut AllocationTest::ordinary(),
            #[cfg(feature = "mm_selftest")]
            &mut frame_zeroed,
        )
    }

    #[cfg(feature = "mm_selftest")]
    fn allocate(
        layout: Layout,
        #[cfg(feature = "mm_selftest")] test: &mut AllocationTest,
    ) -> Result<Self, MemoryAllocationError> {
        Self::allocate_with(layout, test, &mut frame_zeroed)
    }

    #[cfg(feature = "mm_selftest")]
    fn allocate_with(
        layout: Layout,
        #[cfg(feature = "mm_selftest")] test: &mut AllocationTest,
        #[cfg(feature = "mm_selftest")] data_frame: &mut dyn FnMut()
            -> Result<PhysAddr, SystemError>,
    ) -> Result<Self, MemoryAllocationError> {
        if !task_context() {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK.into());
        }
        Self::allocate_with_owner(
            layout,
            &object_for_task(&ProcessManager::current_pcb()),
            #[cfg(feature = "mm_selftest")]
            test,
            #[cfg(feature = "mm_selftest")]
            data_frame,
        )
    }

    fn allocate_with_owner(
        layout: Layout,
        owner: &Arc<ObjectAccount>,
        #[cfg(feature = "mm_selftest")] test: &mut AllocationTest,
        #[cfg(feature = "mm_selftest")] data_frame: &mut dyn FnMut()
            -> Result<PhysAddr, SystemError>,
    ) -> Result<Self, MemoryAllocationError> {
        if !task_context() {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK.into());
        }
        if !Self::is_supported() {
            return Err(SystemError::ENOSYS.into());
        }
        if layout.size() == 0 {
            return Err(SystemError::EINVAL.into());
        }
        let pages = layout
            .size()
            .checked_add(MMArch::PAGE_SIZE - 1)
            .ok_or(SystemError::EOVERFLOW)?
            / MMArch::PAGE_SIZE;
        if pages > PAGES || layout.align() > SIZE {
            return Err(SystemError::ENOMEM.into());
        }
        Self::allocate_owned(
            layout,
            pages,
            #[cfg(feature = "mm_selftest")]
            test,
            #[cfg(feature = "mm_selftest")]
            data_frame,
            owner,
        )
    }

    #[cfg(feature = "mm_selftest")]
    pub(super) fn try_new_zeroed_in_with(
        layout: Layout,
        owner: &Arc<ObjectAccount>,
        data_frame: &mut dyn FnMut() -> Result<PhysAddr, SystemError>,
    ) -> Result<Self, MemoryAllocationError> {
        Self::allocate_with_owner(layout, owner, &mut AllocationTest::ordinary(), data_frame)
    }

    fn allocate_owned(
        layout: Layout,
        pages: usize,
        #[cfg(feature = "mm_selftest")] test: &mut AllocationTest,
        #[cfg(feature = "mm_selftest")] data_frame: &mut dyn FnMut()
            -> Result<PhysAddr, SystemError>,
        owner: &Arc<ObjectAccount>,
    ) -> Result<Self, MemoryAllocationError> {
        #[cfg(feature = "mm_selftest")]
        test.step()?;
        let charge =
            owner.try_charge(KernelByteKind::Vmalloc, (pages * MMArch::PAGE_SIZE) as u64)?;
        let start = POOL
            .lock_irqsave()
            .reserve(
                pages,
                layout.align().max(MMArch::PAGE_SIZE),
                #[cfg(feature = "mm_selftest")]
                test.isolated,
            )
            .ok_or(SystemError::ENOMEM)?;
        let mut result = Self {
            start,
            pages,
            len: layout.size(),
            chunks: None,
            _charge: charge,
        };
        for offset in (0..pages).step_by(CHUNK) {
            #[cfg(feature = "mm_selftest")]
            test.step()?;
            let metadata = OwnedBox::<Chunk>::prepare(owner, KernelByteKind::SlabUnreclaimable)?;
            #[cfg(feature = "mm_selftest")]
            test.step()?;
            let mut chunk = metadata
                .try_init(Chunk {
                    next: None,
                    offset,
                    frames: [PhysAddr::new(0); CHUNK],
                    acquired: 0,
                    mapped: 0,
                    retired: [None, None],
                })
                .map_err(|_| SystemError::ENOMEM)?;
            chunk.next = result.chunks.take();
            result.chunks = Some(chunk);
            let chunk = result.chunks.as_mut().unwrap();
            for index in 0..CHUNK.min(pages - offset) {
                #[cfg(feature = "mm_selftest")]
                test.step()?;
                #[cfg(feature = "mm_selftest")]
                let frame = data_frame()?;
                #[cfg(not(feature = "mm_selftest"))]
                let frame = frame_zeroed()?;
                chunk.frames[index] = frame;
                chunk.acquired += 1;
                let page = start + offset + index;
                let address = VirtAddr::new(BASE + page * MMArch::PAGE_SIZE);
                Self::map_page(
                    page / TABLE_PAGES,
                    address,
                    chunk.frames[index],
                    owner,
                    #[cfg(feature = "mm_selftest")]
                    test,
                )?;
                chunk.mapped += 1;
            }
        }
        // Also invalidate cached non-present translations before publishing on Sv39.
        flush_tlb_kernel_range(result.address(), result.end(), false);
        Ok(result)
    }

    fn map_page(
        slot: usize,
        address: VirtAddr,
        data: PhysAddr,
        owner: &Arc<ObjectAccount>,
        #[cfg(feature = "mm_selftest")] test: &mut AllocationTest,
    ) -> Result<(), MemoryAllocationError> {
        let map = |mapper: &mut crate::arch::mm::PageMapper| {
            let flush = unsafe {
                mapper.map_phys(
                    address,
                    data,
                    EntryFlags::new()
                        .set_user(false)
                        .set_write(true)
                        .set_execute(false)
                        .set_page_global(false),
                )
            }
            .expect("prepared vmalloc leaf table disappeared");
            unsafe { flush.ignore() };
        };
        {
            // Keep this guard through map(): teardown detaches empty tables
            // under the same POOL lock. On return, mapper drops before pool.
            let pool = POOL.lock_irqsave();
            if pool.tables[slot].is_some() {
                #[cfg(feature = "mm_selftest")]
                test.step()?;
                let mut mapper = KernelMapper::lock();
                map(mapper.as_mut().expect("recursive vmalloc mapper"));
                return Ok(());
            }
        }
        #[cfg(feature = "mm_selftest")]
        test.step()?;
        let charge = owner.try_charge(KernelByteKind::PageTables, MMArch::PAGE_SIZE as u64)?;
        #[cfg(feature = "mm_selftest")]
        test.step()?;
        let frame = frame_zeroed()?;
        let mut unused = Some((frame, charge));
        #[cfg(feature = "mm_selftest")]
        let result;
        {
            let mut pool = POOL.lock_irqsave();
            let mut mapper = KernelMapper::lock();
            let mapper = mapper.as_mut().expect("recursive vmalloc table install");
            if pool.tables[slot].is_none() {
                let (frame, charge) = unused.take().unwrap();
                let record = unsafe { mapper.install_kernel_leaf_table(address, frame) }
                    .expect("vmalloc table slot mismatch");
                pool.tables[slot] = Some(LeafTable {
                    record,
                    frame,
                    _charge: charge,
                });
            }
            #[cfg(feature = "mm_selftest")]
            {
                result = test.step();
                if result.is_ok() {
                    map(mapper);
                }
            }
            #[cfg(not(feature = "mm_selftest"))]
            map(mapper);
        }
        if let Some((frame, charge)) = unused {
            unsafe { LockedFrameAllocator.free_one(frame) };
            drop(charge);
        }
        #[cfg(feature = "mm_selftest")]
        return result.map_err(Into::into);
        #[cfg(not(feature = "mm_selftest"))]
        Ok(())
    }

    pub fn address(&self) -> VirtAddr {
        VirtAddr::new(BASE + self.start * MMArch::PAGE_SIZE)
    }
    fn end(&self) -> VirtAddr {
        self.address() + self.pages * MMArch::PAGE_SIZE
    }
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn as_slice(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self.address().as_ptr::<u8>(), self.len) }
    }
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(self.address().as_ptr::<u8>(), self.len) }
    }
}

impl Drop for VmAllocation {
    fn drop(&mut self) {
        assert!(
            task_context(),
            "vmalloc Drop requires sleepable task context"
        );
        let mut freed_tables = false;
        let mut current = self.chunks.as_mut();
        while let Some(chunk) = current {
            let mut pool = POOL.lock_irqsave();
            let mut mapper = KernelMapper::lock();
            let mapper = mapper.as_mut().expect("recursive vmalloc teardown");
            for index in 0..chunk.mapped {
                let address = self.start + chunk.offset + index;
                let cleared = unsafe {
                    mapper.clear_mapping_at_level(
                        VirtAddr::new(BASE + address * MMArch::PAGE_SIZE),
                        0,
                    )
                };
                assert_eq!(cleared.map(|(frame, _)| frame), Some(chunk.frames[index]));
            }
            // Include a newly installed empty table when mapping failed before its first leaf.
            let first_slot = (self.start + chunk.offset) / TABLE_PAGES;
            let last_slot =
                (self.start + chunk.offset + chunk.acquired.saturating_sub(1)) / TABLE_PAGES;
            for (retired, slot) in (first_slot..=last_slot).enumerate() {
                if pool.tables[slot]
                    .as_ref()
                    .is_some_and(|table| unsafe { mapper.detach_empty_kernel_table(&table.record) })
                {
                    chunk.retired[retired] = pool.tables[slot].take();
                    freed_tables = true;
                }
            }
            current = chunk.next.as_mut();
        }
        flush_tlb_kernel_range(self.address(), self.end(), freed_tables);
        // No recursion and no allocations in destruction, even for the entire VA pool.
        while let Some(mut chunk) = self.chunks.take() {
            self.chunks = chunk.next.take();
            for frame in &chunk.frames[..chunk.acquired] {
                unsafe { LockedFrameAllocator.free_one(*frame) };
            }
            drop(chunk);
        }
        let mut pool = POOL.lock_irqsave();
        pool.set_range(self.start, self.pages, false);
    }
}

#[cfg(feature = "mm_selftest")]
mod selftest;
#[cfg(feature = "mm_selftest")]
pub(crate) use selftest::report as selftest_report;
#[cfg(feature = "mm_selftest")]
use selftest::AllocationTest;

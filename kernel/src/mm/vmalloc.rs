//! Sparse, virtually contiguous ordinary RAM ownership.
//!
//! Shared upper tables are published before user roots exist. The raw backend
//! uses only non-recursive local metadata storage; interrupt-side destruction
//! transfers its existing record to the permanent memory notification worker.

#[cfg(feature = "mm_selftest")]
use crate::cgroup::memory::object_for_task;
use crate::{
    arch::{mm::LockedFrameAllocator, CurrentIrqArch, MMArch},
    cgroup::memory::{
        accounting::KernelByteKind,
        object::{ObjectAccount, ObjectCharge},
    },
    exception::{in_interrupt, InterruptArch},
    libs::{
        rbtree::{PreparedNode, RBTree},
        spinlock::SpinLock,
    },
    mm::{
        allocation::MemoryAllocationError,
        allocator::{
            heap::AllocationCharge,
            kernel_allocator::allocation_footprint,
            page_frame::{FrameAllocator, PageFrameCount},
        },
        kernel_mapper::KernelMapper,
        page::{CreatedPageTable, EntryFlags},
        tlb::flush_tlb_kernel_range,
        MemoryManagementArch, PhysAddr, VirtAddr,
    },
    process::ProcessManager,
    KERNEL_ALLOCATOR,
};
use alloc::{boxed::Box, sync::Arc};
use core::{
    alloc::Layout,
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};
use system_error::SystemError;

#[cfg(target_arch = "x86_64")]
const BASE: usize = 0xffff_a200_0000_0000;
#[cfg(target_arch = "x86_64")]
const SIZE: usize = 128 << 30;
#[cfg(target_arch = "riscv64")]
const BASE: usize = 0xffff_fffb_8000_0000;
#[cfg(target_arch = "riscv64")]
const SIZE: usize = 16 << 30;
const PAGES: usize = SIZE / MMArch::PAGE_SIZE;
const TABLE_PAGES: usize = MMArch::PAGE_ENTRY_NUM;
const CHUNK: usize = 64;
#[cfg(feature = "mm_selftest")]
const SELFTEST_PAGES: usize = 4 * TABLE_PAGES;
#[cfg(feature = "mm_selftest")]
const SELFTEST_START: usize = PAGES - SELFTEST_PAGES;
#[cfg(feature = "mm_selftest")]
const SELFTEST_RESERVED_START: usize = PAGES - (2 << 30) / MMArch::PAGE_SIZE;
static READY: AtomicBool = AtomicBool::new(false);
// Like Linux nr_vmalloc_pages, count owned RAM through partial construction
// and deferred retirement. VA padding, metadata and page tables are separate.
static DATA_PAGES: AtomicU64 = AtomicU64::new(0);

pub(crate) fn usage_bytes() -> u64 {
    DATA_PAGES.load(Ordering::Relaxed) * MMArch::PAGE_SIZE as u64
}

pub(crate) const fn capacity_bytes() -> u64 {
    SIZE as u64
}

pub fn contains(address: VirtAddr) -> bool {
    address.data().wrapping_sub(BASE) < SIZE
}

/// Whether a nonempty half-open interval intersects the reserved vmalloc arena.
pub(crate) fn overlaps(start: VirtAddr, end: VirtAddr) -> bool {
    start < end && start.data() < BASE + SIZE && end.data() > BASE
}

unsafe fn metadata_box<T>(value: T) -> Result<Box<T>, SystemError> {
    let pointer = KERNEL_ALLOCATOR
        .alloc_metadata(Layout::new::<T>())
        .cast::<T>();
    if pointer.is_null() {
        return Err(SystemError::ENOMEM);
    }
    pointer.write(value);
    Ok(Box::from_raw(pointer))
}

fn charge(
    owner: Option<&Arc<ObjectAccount>>,
    kind: KernelByteKind,
    bytes: u64,
) -> Result<Option<ObjectCharge>, MemoryAllocationError> {
    owner
        .map(|owner| owner.try_charge(kind, bytes))
        .transpose()
        .map_err(Into::into)
}

#[derive(Debug)]
struct Reservation {
    end: usize,
    charge: Option<ObjectCharge>,
}
type ReservationNode = PreparedNode<usize, Reservation>;
type TableNode = PreparedNode<usize, LeafTable>;

#[derive(Debug)]
struct LeafTable {
    record: Option<CreatedPageTable>,
    frame: PhysAddr,
    _charge: Option<ObjectCharge>,
    _node_charge: Option<ObjectCharge>,
}
impl Drop for LeafTable {
    fn drop(&mut self) {
        unsafe { LockedFrameAllocator.free_one(self.frame) };
    }
}
fn free_table(node: TableNode) {
    // Deallocate the index node before returning its independent receipt.
    let (_, table) = node.into_pair();
    drop(table);
}
struct Pool {
    occupied: RBTree<usize, Reservation>,
    tables: RBTree<usize, LeafTable>,
    ready: bool,
}
static POOL: SpinLock<Pool> = SpinLock::new(Pool {
    occupied: RBTree::new(),
    tables: RBTree::new(),
    ready: false,
});

impl Pool {
    fn find_free(
        &self,
        pages: usize,
        align: usize,
        payload_offset: usize,
        low: usize,
        high: usize,
    ) -> Option<usize> {
        if !self.ready || pages > high.checked_sub(low)? {
            return None;
        }
        let mut start = low;
        for (&occupied, reservation) in self.occupied.iter() {
            if reservation.end <= low {
                continue;
            }
            if occupied >= high {
                break;
            }
            let candidate = Self::aligned(start, align, payload_offset)?;
            if candidate.checked_add(pages)? <= occupied.min(high) {
                return Some(candidate);
            }
            start = start.max(reservation.end);
        }
        let candidate = Self::aligned(start, align, payload_offset)?;
        (candidate.checked_add(pages)? <= high).then_some(candidate)
    }
    fn aligned(start: usize, align: usize, offset: usize) -> Option<usize> {
        let address = BASE.checked_add(start.checked_mul(MMArch::PAGE_SIZE)?)?;
        // Sub-page payload alignment is already satisfied by its prefix;
        // align the mapping itself. Over-page alignment biases by the header
        // page so the payload, rather than the preceding mapping, is aligned.
        let bias = if align > MMArch::PAGE_SIZE { offset } else { 0 };
        let payload = address.checked_add(bias)?.checked_add(align - 1)? & !(align - 1);
        let mapped = payload.checked_sub(bias)?;
        debug_assert_eq!(mapped % MMArch::PAGE_SIZE, 0);
        Some(mapped.checked_sub(BASE)? / MMArch::PAGE_SIZE)
    }
    fn reserve(
        &mut self,
        pages: usize,
        align: usize,
        offset: usize,
        mut node: ReservationNode,
        #[cfg(feature = "mm_selftest")] isolated: bool,
        #[cfg(feature = "mm_selftest")] bounds: Option<(usize, usize)>,
    ) -> Result<usize, ReservationNode> {
        #[cfg(feature = "mm_selftest")]
        let (low, high) = if isolated {
            let (low, high) = bounds.unwrap_or((SELFTEST_START, PAGES));
            if low < SELFTEST_RESERVED_START || high > PAGES || low > high {
                return Err(node);
            }
            (low, high)
        } else {
            (0, SELFTEST_RESERVED_START)
        };
        #[cfg(not(feature = "mm_selftest"))]
        let (low, high) = (0, PAGES);
        let Some(start) = self.find_free(pages, align, offset, low, high) else {
            return Err(node);
        };
        *node.key_mut() = start;
        node.value_mut().end = start + pages;
        self.occupied.insert_prepared(node);
        Ok(start)
    }
}

/// All shared level-1 parents remain pinned for the lifetime of the kernel.
pub(crate) unsafe fn init() {
    let end = BASE.checked_add(SIZE).expect("vmalloc VA overflow");
    assert!(MMArch::virt_is_valid(VirtAddr::new(BASE)) && !VirtAddr::new(BASE).check_user());
    assert!(MMArch::virt_is_valid(VirtAddr::new(end - 1)) && !VirtAddr::new(end - 1).check_user());
    assert!(end <= MMArch::MMIO_BASE.data() || BASE >= MMArch::MMIO_TOP.data());
    assert!(end <= MMArch::FIXMAP_START_VADDR.data() || BASE >= MMArch::FIXMAP_END_VADDR.data());
    let blocks = crate::mm::memblock::mem_block_manager();
    for index in 0..blocks.total_initial_memory_regions() {
        let area = blocks.get_initial_memory_region(index).unwrap();
        let start = area
            .base
            .data()
            .checked_add(MMArch::PHYS_OFFSET)
            .expect("RAM direct map overflow");
        let ram_end = start
            .checked_add(area.size)
            .expect("RAM direct map overflow");
        assert!(ram_end <= BASE || start >= end, "RAM direct map [{start:#x}, {ram_end:#x}) overlaps reserved vmalloc [{BASE:#x}, {end:#x}); RAM is not truncated");
    }
    let _irq = CurrentIrqArch::save_and_disable_irq();
    let mut pool = POOL.lock_irqsave();
    let mut mapper = KernelMapper::lock();
    for address in (BASE..end).step_by(1 << 30) {
        mapper
            .as_mut()
            .expect("recursive vmalloc init")
            .prepare_kernel_subtree(VirtAddr::new(address), 1)
            .expect("cannot prepare vmalloc shared subtree");
    }
    pool.ready = true;
    READY.store(true, Ordering::Release);
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

struct Chunk {
    next: Option<Box<Chunk>>,
    offset: usize,
    frames: [PhysAddr; CHUNK],
    acquired: usize,
    mapped: usize,
    retired: [Option<TableNode>; 2],
    charge: Option<ObjectCharge>,
}

pub(crate) struct AllocationRecord {
    start: usize,
    pages: usize,
    len: usize,
    payload_offset: usize,
    reserved: bool,
    chunks: Option<Box<Chunk>>,
    charge: Option<ObjectCharge>,
    metadata_charge: Option<ObjectCharge>,
    attached_charge: Option<AllocationCharge>,
    next_retired: usize,
    #[cfg(all(feature = "mm_selftest", target_arch = "x86_64"))]
    teardown_observer: Option<Arc<crate::mm::tlb::selftest::Observer>>,
}

/// Explicit owner; its handle needs no header in the mapped allocation.
pub(crate) struct VmAllocation {
    record: Option<Box<AllocationRecord>>,
}
impl Deref for VmAllocation {
    type Target = AllocationRecord;
    fn deref(&self) -> &Self::Target {
        self.record.as_ref().unwrap()
    }
}
impl DerefMut for VmAllocation {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.record.as_mut().unwrap()
    }
}

pub(crate) fn task_context() -> bool {
    CurrentIrqArch::is_irq_enabled()
        && !in_interrupt()
        && ProcessManager::initialized()
        && ProcessManager::current_pcb().preempt_count() == 0
}

#[derive(Clone, Copy)]
struct Plan {
    pages: usize,
    len: usize,
    align: usize,
    offset: usize,
}
impl Plan {
    fn new(layout: Layout, header: bool) -> Option<Self> {
        if layout.size() == 0 {
            return None;
        }
        let align = layout.align().max(MMArch::PAGE_SIZE);
        if align > SIZE {
            return None;
        }
        // A page-aligned payload uses the preceding page for its header. A
        // smaller alignment only needs the first header-sized prefix.
        let offset = if header {
            if layout.align() >= MMArch::PAGE_SIZE {
                MMArch::PAGE_SIZE
            } else {
                core::mem::size_of::<usize>().checked_add(layout.align() - 1)?
                    & !(layout.align() - 1)
            }
        } else {
            0
        };
        let pages = layout
            .size()
            .checked_add(offset)?
            .checked_add(MMArch::PAGE_SIZE - 1)?
            / MMArch::PAGE_SIZE;
        (pages <= PAGES).then_some(Self {
            pages,
            len: layout.size(),
            align,
            offset,
        })
    }
    fn footprint(self) -> Option<u64> {
        let data = self.pages.checked_mul(MMArch::PAGE_SIZE)? as u64;
        let chunks = self.pages.div_ceil(CHUNK) as u64;
        data.checked_add(allocation_footprint(Layout::new::<AllocationRecord>()))?
            .checked_add(chunks.checked_mul(allocation_footprint(Layout::new::<Chunk>()))?)?
            .checked_add(allocation_footprint(ReservationNode::allocation_layout()))
    }
}

impl VmAllocation {
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
    fn allocate(layout: Layout, test: &mut AllocationTest) -> Result<Self, MemoryAllocationError> {
        Self::allocate_with(layout, test, &mut frame_zeroed)
    }
    #[cfg(feature = "mm_selftest")]
    fn allocate_with(
        layout: Layout,
        test: &mut AllocationTest,
        data_frame: &mut dyn FnMut() -> Result<PhysAddr, SystemError>,
    ) -> Result<Self, MemoryAllocationError> {
        if !task_context() {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK.into());
        }
        Self::allocate_with_owner(
            layout,
            &object_for_task(&ProcessManager::current_pcb()),
            test,
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
        let plan = Plan::new(layout, false).ok_or(SystemError::ENOMEM)?;
        Self::allocate_owned(
            layout,
            plan.pages,
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
        let plan = Plan {
            pages,
            len: layout.size(),
            align: layout.align().max(MMArch::PAGE_SIZE),
            offset: 0,
        };
        allocate_record(
            plan,
            Some(owner),
            #[cfg(feature = "mm_selftest")]
            test,
            #[cfg(feature = "mm_selftest")]
            data_frame,
        )
        .map(|record| Self {
            record: Some(record),
        })
    }
    #[cfg(all(feature = "mm_selftest", target_arch = "x86_64"))]
    pub(crate) fn set_teardown_observer(
        &mut self,
        observer: Arc<crate::mm::tlb::selftest::Observer>,
    ) {
        assert_eq!(self.pages, 1);
        observer.frame.store(
            self.chunks.as_ref().unwrap().frames[0].data(),
            Ordering::Release,
        );
        self.teardown_observer = Some(observer);
    }
    pub fn address(&self) -> VirtAddr {
        self.deref().address()
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
        if let Some(record) = self.record.take() {
            retire(record);
        }
    }
}
impl AllocationRecord {
    fn mapped_start(&self) -> VirtAddr {
        VirtAddr::new(BASE + self.start * MMArch::PAGE_SIZE)
    }
    fn address(&self) -> VirtAddr {
        self.mapped_start() + self.payload_offset
    }
    fn end(&self) -> VirtAddr {
        self.mapped_start() + self.pages * MMArch::PAGE_SIZE
    }
}

fn allocate_record(
    plan: Plan,
    owner: Option<&Arc<ObjectAccount>>,
    #[cfg(feature = "mm_selftest")] test: &mut AllocationTest,
    #[cfg(feature = "mm_selftest")] data_frame: &mut dyn FnMut() -> Result<PhysAddr, SystemError>,
) -> Result<Box<AllocationRecord>, MemoryAllocationError> {
    #[cfg(feature = "mm_selftest")]
    test.step()?;
    let data_charge = charge(
        owner,
        KernelByteKind::Vmalloc,
        (plan.pages * MMArch::PAGE_SIZE) as u64,
    )?;
    let record_charge = charge(
        owner,
        KernelByteKind::SlabUnreclaimable,
        allocation_footprint(Layout::new::<AllocationRecord>()),
    )?;
    #[cfg(feature = "mm_selftest")]
    test.step()?;
    let record = unsafe {
        metadata_box(AllocationRecord {
            start: 0,
            pages: plan.pages,
            len: plan.len,
            payload_offset: plan.offset,
            reserved: false,
            chunks: None,
            charge: data_charge,
            metadata_charge: record_charge,
            attached_charge: None,
            next_retired: 0,
            #[cfg(all(feature = "mm_selftest", target_arch = "x86_64"))]
            teardown_observer: None,
        })
    }?;
    // The guard also retires partial mappings on every fallible exit below.
    let mut result = VmAllocation {
        record: Some(record),
    };
    let reservation_charge = charge(
        owner,
        KernelByteKind::SlabUnreclaimable,
        allocation_footprint(ReservationNode::allocation_layout()),
    )?;
    #[cfg(feature = "mm_selftest")]
    test.step()?;
    let node = unsafe {
        ReservationNode::try_new_with(
            0,
            Reservation {
                end: 0,
                charge: reservation_charge,
            },
            |layout| KERNEL_ALLOCATOR.alloc_metadata(layout),
        )
    }
    .map_err(|_| SystemError::ENOMEM)?;
    let reservation = {
        POOL.lock_irqsave().reserve(
            plan.pages,
            plan.align,
            plan.offset,
            node,
            #[cfg(feature = "mm_selftest")]
            test.isolated,
            #[cfg(feature = "mm_selftest")]
            test.bounds,
        )
    };
    let start = match reservation {
        Ok(start) => start,
        Err(node) => {
            let (_, reservation) = node.into_pair();
            drop(reservation);
            return Err(SystemError::ENOMEM.into());
        }
    };
    result.start = start;
    result.reserved = true;
    for offset in (0..plan.pages).step_by(CHUNK) {
        #[cfg(feature = "mm_selftest")]
        test.step()?;
        let metadata_charge = charge(
            owner,
            KernelByteKind::SlabUnreclaimable,
            allocation_footprint(Layout::new::<Chunk>()),
        )?;
        #[cfg(feature = "mm_selftest")]
        test.step()?;
        let mut chunk = unsafe {
            metadata_box(Chunk {
                next: None,
                offset,
                frames: [PhysAddr::new(0); CHUNK],
                acquired: 0,
                mapped: 0,
                retired: [None, None],
                charge: metadata_charge,
            })
        }?;
        chunk.next = result.chunks.take();
        result.chunks = Some(chunk);
        let chunk = result.chunks.as_mut().unwrap();
        for index in 0..CHUNK.min(plan.pages - offset) {
            #[cfg(feature = "mm_selftest")]
            test.step()?;
            #[cfg(feature = "mm_selftest")]
            let frame = data_frame()?;
            #[cfg(not(feature = "mm_selftest"))]
            let frame = frame_zeroed()?;
            chunk.frames[index] = frame;
            chunk.acquired += 1;
            DATA_PAGES.fetch_add(1, Ordering::Relaxed);
            let page = start + offset + index;
            map_page(
                page / TABLE_PAGES,
                VirtAddr::new(BASE + page * MMArch::PAGE_SIZE),
                frame,
                owner,
                #[cfg(feature = "mm_selftest")]
                test,
            )?;
            chunk.mapped += 1;
        }
    }
    flush_tlb_kernel_range(result.mapped_start(), result.end(), false);
    Ok(result.record.take().unwrap())
}

fn map_page(
    slot: usize,
    address: VirtAddr,
    data: PhysAddr,
    owner: Option<&Arc<ObjectAccount>>,
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
        let pool = POOL.lock_irqsave();
        if pool.tables.contains_key(&slot) {
            #[cfg(feature = "mm_selftest")]
            test.step()?;
            let mut mapper = KernelMapper::lock();
            map(mapper.as_mut().expect("recursive vmalloc mapper"));
            return Ok(());
        }
    }
    #[cfg(feature = "mm_selftest")]
    test.step()?;
    let page_charge = charge(owner, KernelByteKind::PageTables, MMArch::PAGE_SIZE as u64)?;
    let node_charge = charge(
        owner,
        KernelByteKind::SlabUnreclaimable,
        allocation_footprint(TableNode::allocation_layout()),
    )?;
    #[cfg(feature = "mm_selftest")]
    test.step()?;
    let frame = frame_zeroed()?;
    let table = LeafTable {
        record: None,
        frame,
        _charge: page_charge,
        _node_charge: node_charge,
    };
    let mut unused = Some(
        unsafe {
            TableNode::try_new_with(slot, table, |layout| {
                KERNEL_ALLOCATOR.alloc_metadata(layout)
            })
        }
        .map_err(|_| SystemError::ENOMEM)?,
    );
    let result;
    {
        let mut pool = POOL.lock_irqsave();
        let mut mapper = KernelMapper::lock();
        let mapper = mapper.as_mut().expect("recursive vmalloc table install");
        if !pool.tables.contains_key(&slot) {
            let mut node = unused.take().unwrap();
            let frame = node.value_mut().frame;
            node.value_mut().record = Some(
                unsafe { mapper.install_kernel_leaf_table(address, frame) }
                    .expect("vmalloc table slot mismatch"),
            );
            pool.tables.insert_prepared(node);
        }
        #[cfg(feature = "mm_selftest")]
        {
            result = test.step();
        }
        #[cfg(not(feature = "mm_selftest"))]
        {
            result = Ok::<(), SystemError>(());
        }
        if result.is_ok() {
            map(mapper);
        }
    }
    if let Some(node) = unused {
        free_table(node);
    }
    result.map_err(Into::into)
}

fn destroy(mut record: Box<AllocationRecord>) {
    assert!(
        task_context(),
        "raw vmalloc retirement requires task context"
    );
    if record.reserved {
        let allocation_start = record.start;
        let mut freed_tables = false;
        let mut current = record.chunks.as_mut();
        while let Some(chunk) = current {
            let mut pool = POOL.lock_irqsave();
            let mut mapper = KernelMapper::lock();
            let mapper = mapper.as_mut().expect("recursive vmalloc teardown");
            for index in 0..chunk.mapped {
                let address = allocation_start + chunk.offset + index;
                let cleared = unsafe {
                    mapper.clear_mapping_at_level(
                        VirtAddr::new(BASE + address * MMArch::PAGE_SIZE),
                        0,
                    )
                };
                assert_eq!(cleared.map(|(frame, _)| frame), Some(chunk.frames[index]));
            }
            if chunk.acquired != 0 {
                let first_slot = (allocation_start + chunk.offset) / TABLE_PAGES;
                let last_slot =
                    (allocation_start + chunk.offset + chunk.acquired - 1) / TABLE_PAGES;
                for (retired, slot) in (first_slot..=last_slot).enumerate() {
                    if pool.tables.get(&slot).is_some_and(|table| unsafe {
                        mapper.detach_empty_kernel_table(table.record.as_ref().unwrap())
                    }) {
                        chunk.retired[retired] = pool.tables.extract(&slot);
                        freed_tables = true;
                    }
                }
            }
            current = chunk.next.as_mut();
        }
        #[cfg(all(feature = "mm_selftest", target_arch = "x86_64"))]
        if let Some(observer) = &record.teardown_observer {
            let absent = {
                KernelMapper::lock()
                    .as_ref()
                    .translate(record.address())
                    .is_none()
            };
            observer.cleared(absent);
            if !observer.skip_flush() {
                crate::mm::tlb::flush_tlb_kernel_range_observed(
                    record.mapped_start(),
                    record.end(),
                    freed_tables,
                    observer.clone(),
                );
            }
            if !observer.before_free() {
                crate::mm::tlb::flush_tlb_kernel_range_observed(
                    record.mapped_start(),
                    record.end(),
                    freed_tables,
                    observer.clone(),
                );
            }
        } else {
            flush_tlb_kernel_range(record.mapped_start(), record.end(), freed_tables);
        }
        #[cfg(not(all(feature = "mm_selftest", target_arch = "x86_64")))]
        flush_tlb_kernel_range(record.mapped_start(), record.end(), freed_tables);
        while let Some(mut chunk) = record.chunks.take() {
            record.chunks = chunk.next.take();
            for frame in &chunk.frames[..chunk.acquired] {
                #[cfg(all(feature = "mm_selftest", target_arch = "x86_64"))]
                if let Some(observer) = &record.teardown_observer {
                    assert!(observer.before_free());
                }
                unsafe { LockedFrameAllocator.free_one(*frame) };
                DATA_PAGES.fetch_sub(1, Ordering::Relaxed);
                #[cfg(all(feature = "mm_selftest", target_arch = "x86_64"))]
                if let Some(observer) = &record.teardown_observer {
                    observer.frame_freed();
                }
            }
            for retired in &mut chunk.retired {
                if let Some(node) = retired.take() {
                    #[cfg(all(feature = "mm_selftest", target_arch = "x86_64"))]
                    if let Some(observer) = &record.teardown_observer {
                        assert!(observer.before_free());
                    }
                    free_table(node);
                    #[cfg(all(feature = "mm_selftest", target_arch = "x86_64"))]
                    if let Some(observer) = &record.teardown_observer {
                        observer.tables_freed();
                    }
                }
            }
            let charge = chunk.charge.take();
            drop(chunk);
            drop(charge);
        }
        let node = {
            POOL.lock_irqsave()
                .occupied
                .extract(&record.start)
                .expect("vmalloc reservation disappeared")
        };
        let (_, reservation) = node.into_pair();
        drop(reservation);
    }
    let metadata_charge = record.metadata_charge.take();
    let data_charge = record.charge.take();
    let attached_charge = record.attached_charge.take();
    drop(record);
    drop(metadata_charge);
    drop(data_charge);
    drop(attached_charge);
}

// Metadata must stay within the explicitly provisioned local size classes.
const _: () = assert!(core::mem::size_of::<Chunk>() <= 2048);
const _: () = assert!(core::mem::size_of::<AllocationRecord>() <= 2048);
const _: () = assert!(ReservationNode::allocation_layout().size() <= 2048);
const _: () = assert!(TableNode::allocation_layout().size() <= 2048);

// FIFO prevents new producers from indefinitely postponing an older owner's
// pages and receipt. Every link operation is serialized by RETIRED's IRQ lock.
struct RetiredQueue {
    head: usize,
    tail: usize,
}

impl RetiredQueue {
    const fn new() -> Self {
        Self { head: 0, tail: 0 }
    }

    fn push(&mut self, mut record: Box<AllocationRecord>) {
        record.next_retired = 0;
        let pointer = Box::into_raw(record);
        if self.tail == 0 {
            debug_assert_eq!(self.head, 0);
            self.head = pointer as usize;
        } else {
            // The queue owns its tail until pop transfers that Box out.
            unsafe {
                (*(self.tail as *mut AllocationRecord)).next_retired = pointer as usize;
            }
        }
        self.tail = pointer as usize;
    }

    fn pop(&mut self) -> Option<Box<AllocationRecord>> {
        if self.head == 0 {
            debug_assert_eq!(self.tail, 0);
            return None;
        }
        // The queue uniquely owns every linked Box; unlink before returning
        // ownership, so its destructor never runs while holding the queue lock.
        let mut record = unsafe { Box::from_raw(self.head as *mut AllocationRecord) };
        self.head = record.next_retired;
        if self.head == 0 {
            self.tail = 0;
        }
        record.next_retired = 0;
        Some(record)
    }

    fn is_empty(&self) -> bool {
        self.head == 0
    }
}

static RETIRED: SpinLock<RetiredQueue> = SpinLock::new(RetiredQueue::new());
fn retire(record: Box<AllocationRecord>) {
    if task_context() {
        destroy(record);
    } else {
        {
            RETIRED.lock_irqsave().push(record);
        }
        crate::mm::memory_notify::request_deferred_drop();
    }
}
pub(crate) fn drain_retired() -> bool {
    for _ in 0..32 {
        let record = { RETIRED.lock_irqsave().pop() };
        let Some(record) = record else {
            return false;
        };
        destroy(record);
        crate::sched::cond_resched();
    }
    !RETIRED.lock_irqsave().is_empty()
}

pub(crate) fn global_ready() -> bool {
    READY.load(Ordering::Acquire)
        && ProcessManager::initialized()
        && crate::mm::memory_notify::is_ready()
        && VmAllocation::is_supported()
}
pub(crate) fn global_footprint(layout: Layout) -> Option<u64> {
    Plan::new(layout, true)?.footprint()
}
pub(crate) fn try_global_alloc(layout: Layout) -> *mut u8 {
    if !global_ready() || !task_context() {
        return core::ptr::null_mut();
    }
    let Some(plan) = Plan::new(layout, true) else {
        return core::ptr::null_mut();
    };
    let record = allocate_record(
        plan,
        None,
        #[cfg(feature = "mm_selftest")]
        &mut AllocationTest::ordinary(),
        #[cfg(feature = "mm_selftest")]
        &mut frame_zeroed,
    );
    let Ok(record) = record else {
        return core::ptr::null_mut();
    };
    let payload = record.address().as_ptr::<u8>();
    let pointer = Box::into_raw(record);
    unsafe {
        payload
            .sub(core::mem::size_of::<usize>())
            .cast::<*mut AllocationRecord>()
            .write_unaligned(pointer);
    }
    payload
}
unsafe fn global_record(pointer: *mut u8) -> *mut AllocationRecord {
    pointer
        .sub(core::mem::size_of::<usize>())
        .cast::<*mut AllocationRecord>()
        .read_unaligned()
}
/// `pointer` must be the unique live payload returned by try_global_alloc.
pub(crate) unsafe fn global_dealloc(pointer: *mut u8) {
    retire(Box::from_raw(global_record(pointer)));
}
/// The caller owns a live allocation returned by try_global_alloc.
pub(crate) unsafe fn actual_global_footprint(pointer: *mut u8) -> u64 {
    let record = &*global_record(pointer);
    Plan {
        pages: record.pages,
        len: record.len,
        align: 1,
        offset: record.payload_offset,
    }
    .footprint()
    .expect("live vmalloc footprint overflow")
}
/// Attach exactly once, before the unique owner releases the payload.
pub(crate) unsafe fn attach_charge(pointer: *mut u8, charge: AllocationCharge) {
    let record = &mut *global_record(pointer);
    assert!(record.attached_charge.is_none(), "duplicate heap receipt");
    record.attached_charge = Some(charge);
}

#[cfg(feature = "mm_selftest")]
mod selftest;
#[cfg(feature = "mm_selftest")]
pub(crate) use selftest::report as selftest_report;
#[cfg(feature = "mm_selftest")]
use selftest::AllocationTest;
#[cfg(all(feature = "mm_selftest", target_arch = "x86_64"))]
pub(crate) fn lock_teardown_fixture() -> crate::libs::mutex::MutexGuard<'static, ()> {
    selftest::TEST_LOCK.lock()
}

// 进程的用户空间内存管理

use core::{
    cmp,
    hash::Hasher,
    intrinsics::unlikely,
    ops::Add,
    sync::atomic::{compiler_fence, AtomicU64, AtomicUsize, Ordering},
};

use alloc::{
    collections::BTreeMap,
    sync::{Arc, Weak},
    vec::Vec,
};
use defer::defer;
use hashbrown::HashMap;
use hashbrown::HashSet;
use ida::IdAllocator;
use log::{error, warn};
use system_error::SystemError;

use crate::{
    arch::{mm::PageMapper, CurrentIrqArch, MMArch},
    exception::InterruptArch,
    filesystem::{
        page_cache::UnmapMappingMode,
        vfs::{
            file::{File, FileMode},
            FileType, InodeId,
        },
    },
    ipc::shm::SysVShmAttach,
    libs::{
        align::page_align_up,
        cpumask::CpuMask,
        mutex::{Mutex, MutexGuard},
        rwsem::{RwSem, RwSemReadGuard, RwSemWriteGuard},
        spinlock::SpinLock,
        wait_queue::WaitQueue,
    },
    mm::{
        mmu_gather::MmuGather,
        page::{page_manager_lock, page_reclaimer_lock},
        PhysAddr,
    },
    process::{
        cred::{capable, CAPFlags},
        resource::RLimitID,
        ProcessManager,
    },
};

use super::{
    allocator::page_frame::{
        deallocate_page_frames, PageFrameCount, PhysPageFrame, VirtPageFrame, VirtPageFrameIter,
    },
    fault::{FaultFlags, PageFaultHandler, PageFaultMessage},
    page::{EntryFlags, Flusher, Page, PageFlags, PageType},
    syscall::{MadvFlags, MapFlags, MremapFlags, ProtFlags},
    MemoryManagementArch, PageTableKind, VirtAddr, VirtRegion, VmFaultReason, VmFlags,
};
use crate::arch::mm::LockedFrameAllocator;

/// MMAP_MIN_ADDR的默认值
/// 以下内容来自linux-5.19:
///  This is the portion of low virtual memory which should be protected
//   from userspace allocation.  Keeping a user from writing to low pages
//   can help reduce the impact of kernel NULL pointer bugs.
//   For most ia64, ppc64 and x86 users with lots of address space
//   a value of 65536 is reasonable and should cause no problems.
//   On arm and other archs it should not be higher than 32768.
//   Programs which use vm86 functionality or have some need to map
//   this low address space will need CAP_SYS_RAWIO or disable this
//   protection by setting the value to 0.
pub const DEFAULT_MMAP_MIN_ADDR: usize = 65536;

/// Linux `security_mmap_addr()`/`cap_mmap_addr()` semantics for low fixed mappings.
///
/// Mapping below `mmap_min_addr` is denied with `EPERM` unless the caller has
/// `CAP_SYS_RAWIO` in the initial user namespace. Non-fixed hints are rounded
/// by the caller and should not enter this helper.
pub fn check_mmap_min_addr(vaddr: VirtAddr, min_vaddr: VirtAddr) -> Result<(), SystemError> {
    if vaddr < min_vaddr && !capable(CAPFlags::CAP_SYS_RAWIO) {
        return Err(SystemError::EPERM);
    }
    Ok(())
}

/// LockedVMA的id分配器
static LOCKEDVMA_ID_ALLOCATOR: SpinLock<IdAllocator> =
    SpinLock::new(IdAllocator::new(0, usize::MAX).unwrap());

/// AddressSpace的全局唯一ID分配器
/// 用于为每个地址空间分配一个全局唯一且递增的ID
static ADDRESS_SPACE_ID_ALLOCATOR: AtomicU64 = AtomicU64::new(1);

pub type MmapReservationId = u64;

static MMAP_RESERVATION_ID_ALLOCATOR: AtomicU64 = AtomicU64::new(1);

pub struct FileMappingWithFileArgs {
    pub file: Arc<File>,
    pub start_vaddr: VirtAddr,
    pub len: usize,
    pub prot_flags: ProtFlags,
    pub map_flags: MapFlags,
    pub may_exec: bool,
    pub offset: usize,
    pub round_to_min: bool,
    pub allocate_at_once: bool,
    pub sysv_shm: Option<Arc<SysVShmAttach>>,
    pub fixed_noreplace_conflict_error_before_mmap_min: Option<SystemError>,
}

#[derive(Debug)]
pub struct AddressSpace {
    /// 全局唯一的地址空间ID，用于标识不同的地址空间
    /// 该ID在地址空间的整个生命周期内保持不变，且永不重复
    id: u64,
    /// 页表物理地址（创建后不变，可无锁访问）
    /// 用于在调度器上下文中快速切换页表，无需获取RwSem锁
    table_paddr: PhysAddr,
    /// The set of CPUs that may currently hold TLB entries for this mm.
    ///
    /// Maintained by context switch / exec / process exit paths; `flush_tlb_mm_range` uses this to determine shootdown targets.
    /// Cf. Linux 6.6 `struct mm_struct::cpu_bitmap` semantics.
    pub active_cpus: SpinLock<CpuMask>,
    /// Monotonically increasing count of page table modifications for this mm.
    ///
    /// `flush_tlb_*` must increment this after publishing page table writes and before snapshotting `active_cpus`;
    /// remote CPUs receiving IPI write this to per-CPU `TlbState::loaded_tlb_gen` as a "caught-up generation" marker.
    pub tlb_gen: AtomicU64,
    /// Serialize all user page-table edits for this mm.
    ///
    /// This is intentionally separate from `inner: RwSem<InnerAddressSpace>`:
    /// file-rmap walkers need to edit remote PTEs under `mapping->i_mmap.read()`
    /// without taking `mm.write()`, while fault/munmap/mprotect/mremap paths must
    /// still synchronize with those edits.
    page_table_edit_lock: Mutex<()>,
    /// Per-mm resident user pages, counted by present user PTEs.
    resident_user_pages: AtomicUsize,
    /// Monotonic OOM reclaim progress generation.
    ///
    /// This is advanced only after unmapped pages have passed the required TLB
    /// shootdown and their deferred `Arc<Page>` references are actually dropped.
    /// OOM waiters must not treat the earlier resident-page accounting decrement
    /// as reclaim progress.
    oom_reclaim_generation: AtomicU64,
    /// 使用RwSem而非RwLock，因为地址空间操作可能需要进行I/O（如页缺失时的文件读取）
    inner: RwSem<InnerAddressSpace>,
    /// 等待未发布的 mmap reservation 提交或取消。
    reservation_wait: WaitQueue,
}

impl AddressSpace {
    pub fn new(create_stack: bool) -> Result<Arc<Self>, SystemError> {
        let inner = InnerAddressSpace::new(false)?;
        let table_paddr = inner.user_mapper.utable.table().phys();
        let id = ADDRESS_SPACE_ID_ALLOCATOR.fetch_add(1, Ordering::Relaxed);
        let result = Arc::new(Self {
            id,
            table_paddr,
            active_cpus: SpinLock::new(CpuMask::new()),
            tlb_gen: AtomicU64::new(0),
            page_table_edit_lock: Mutex::new(()),
            resident_user_pages: AtomicUsize::new(0),
            oom_reclaim_generation: AtomicU64::new(0),
            inner: RwSem::new(inner),
            reservation_wait: WaitQueue::default(),
        });
        // Back-fill the Weak<AddressSpace> so that InnerAddressSpace methods can obtain
        // the outer Arc to construct MmuGather / initiate TLB shootdown.
        {
            let mut g = result.inner.write();
            g.mm_id = id;
            g.outer = Arc::downgrade(&result);
            g.mappings.set_owner(Arc::downgrade(&result));
            if create_stack {
                g.new_user_stack(UserStack::DEFAULT_USER_STACK_SIZE)?;
            }
        }
        return Ok(result);
    }

    /// 获取地址空间的全局唯一ID
    #[inline(always)]
    pub fn id(&self) -> u64 {
        self.id
    }

    /// 获取页表物理地址（无锁访问）
    /// 用于在调度器上下文中快速切换页表
    #[inline(always)]
    pub fn table_paddr(&self) -> PhysAddr {
        self.table_paddr
    }

    /// 从pcb中获取当前进程的地址空间结构体的Arc指针
    pub fn current() -> Result<Arc<AddressSpace>, SystemError> {
        let vm = ProcessManager::current_pcb()
            .basic()
            .user_vm()
            .expect("Current process has no address space");

        return Ok(vm);
    }

    /// 判断某个地址空间是否为当前进程的地址空间
    pub fn is_current(self: &Arc<Self>) -> bool {
        let current = Self::current();
        if let Ok(current) = current {
            return Arc::ptr_eq(&current, self);
        }
        return false;
    }

    /// 将此地址空间的页表设置为当前页表（无锁）
    ///
    /// 此方法用于调度器上下文中的快速页表切换，无需获取RwSem锁。
    /// 安全性由调用者保证：只在进程切换时使用。
    #[inline(always)]
    pub unsafe fn make_current(&self) {
        MMArch::set_table(PageTableKind::User, self.table_paddr);
    }

    /// Add the specified CPU to this mm's `active_cpus`.
    ///
    /// The caller should invoke this after the hardware has loaded this mm's page table, or immediately before loading,
    /// so that `flush_tlb_mm_range` can see this CPU after inc_tlb_gen.
    #[inline]
    pub fn active_cpus_set(&self, cpu: crate::smp::cpu::ProcessorId) {
        let mut g = self.active_cpus.lock();
        g.set(cpu, true);
    }

    /// Remove the specified CPU from `active_cpus`.
    ///
    /// The caller should invoke this before switching the hardware page table to a different mm,
    /// ensuring that any subsequent shootdown will no longer consider this CPU a target.
    #[inline]
    pub fn active_cpus_clear(&self, cpu: crate::smp::cpu::ProcessorId) {
        let mut g = self.active_cpus.lock();
        g.set(cpu, false);
    }

    /// Issue a range-based TLB flush for this mm (including remote shootdown + local).
    ///
    /// See `crate::mm::tlb::flush_tlb_mm_range` for constraints.
    #[inline]
    pub fn flush_tlb_range(
        self: &Arc<Self>,
        start: VirtAddr,
        end: VirtAddr,
        stride_shift: u8,
        freed_tables: bool,
    ) {
        crate::mm::tlb::flush_tlb_mm_range(self, start, end, stride_shift, freed_tables);
    }

    /// Issue a full-mm TLB flush.
    #[inline]
    pub fn flush_tlb_all(self: &Arc<Self>) {
        crate::mm::tlb::flush_tlb_mm(self);
    }

    #[inline]
    pub fn page_table_edit(&self) -> MutexGuard<'_, ()> {
        debug_assert!(
            CurrentIrqArch::is_irq_enabled(),
            "page_table_edit_lock must not be taken with interrupts disabled"
        );
        self.page_table_edit_lock.lock()
    }

    #[inline(always)]
    pub fn resident_pages(&self) -> usize {
        self.resident_user_pages.load(Ordering::Relaxed)
    }

    #[inline(always)]
    pub fn oom_reclaim_generation(&self) -> u64 {
        self.oom_reclaim_generation.load(Ordering::Acquire)
    }

    #[inline(always)]
    pub fn advance_oom_reclaim_generation(&self) {
        self.oom_reclaim_generation.fetch_add(1, Ordering::AcqRel);
    }

    #[inline(always)]
    pub fn account_present_page_add(&self) {
        self.account_present_pages_add(1);
    }

    #[inline(always)]
    pub fn account_present_pages_add(&self, count: usize) {
        if count != 0 {
            self.resident_user_pages.fetch_add(count, Ordering::Relaxed);
        }
    }

    #[inline(always)]
    pub fn account_present_page_sub(&self) {
        self.account_present_pages_sub(1);
    }

    #[inline(always)]
    pub fn account_present_pages_sub(&self, count: usize) {
        if count == 0 {
            return;
        }

        let prev = self
            .resident_user_pages
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |pages| {
                Some(pages.saturating_sub(count))
            })
            .unwrap_or(0);
        if prev < count {
            error!(
                "resident_user_pages underflow on mm id={}, prev={}, sub={}",
                self.id(),
                prev,
                count
            );
        }
        debug_assert!(
            prev >= count,
            "resident_user_pages underflow on mm id={}, prev={}, sub={}",
            self.id(),
            prev,
            count
        );
    }

    pub fn wait_for_no_reservation_conflict(self: &Arc<Self>, region: VirtRegion) {
        self.reservation_wait.wait_until(|| {
            let guard = self.write();
            if guard.mappings.first_reservation_conflict(region).is_none() {
                Some(())
            } else {
                None
            }
        });
    }

    pub fn wait_for_no_reservation_conflict_interruptible(
        self: &Arc<Self>,
        region: VirtRegion,
    ) -> Result<(), SystemError> {
        self.reservation_wait.wait_until_interruptible(|| {
            let guard = self.write();
            if guard.mappings.first_reservation_conflict(region).is_none() {
                Some(())
            } else {
                None
            }
        })
    }

    pub fn wait_for_no_reservations(self: &Arc<Self>) {
        self.reservation_wait.wait_until(|| {
            let guard = self.write();
            if guard.mappings.first_reservation_region().is_none() {
                Some(())
            } else {
                None
            }
        });
    }

    pub fn wait_for_no_reservations_interruptible(self: &Arc<Self>) -> Result<(), SystemError> {
        self.reservation_wait.wait_until_interruptible(|| {
            let guard = self.write();
            if guard.mappings.first_reservation_region().is_none() {
                Some(())
            } else {
                None
            }
        })
    }

    pub fn read_guard_no_reservation_conflict(
        self: &Arc<Self>,
        region: VirtRegion,
    ) -> RwSemReadGuard<'_, InnerAddressSpace> {
        self.reservation_wait.wait_until(|| {
            let guard = self.read();
            if guard.mappings.first_reservation_conflict(region).is_none() {
                Some(guard)
            } else {
                None
            }
        })
    }

    pub fn write_guard_no_reservation_conflict(
        self: &Arc<Self>,
        region: VirtRegion,
    ) -> RwSemWriteGuard<'_, InnerAddressSpace> {
        self.reservation_wait.wait_until(|| {
            let guard = self.write();
            if guard.mappings.first_reservation_conflict(region).is_none() {
                Some(guard)
            } else {
                None
            }
        })
    }

    pub fn read_guard_no_reservations(self: &Arc<Self>) -> RwSemReadGuard<'_, InnerAddressSpace> {
        self.reservation_wait.wait_until(|| {
            let guard = self.read();
            if guard.mappings.first_reservation_region().is_none() {
                Some(guard)
            } else {
                None
            }
        })
    }

    pub fn write_guard_no_reservations(self: &Arc<Self>) -> RwSemWriteGuard<'_, InnerAddressSpace> {
        self.reservation_wait.wait_until(|| {
            let guard = self.write();
            if guard.mappings.first_reservation_region().is_none() {
                Some(guard)
            } else {
                None
            }
        })
    }

    fn wake_reservation_waiters(&self) {
        self.reservation_wait.wake_all();
    }

    fn round_mmap_hint(
        start_vaddr: VirtAddr,
        round_to_min: bool,
        fixed_hint: bool,
    ) -> Option<VirtAddr> {
        let addr = start_vaddr.data() & (!MMArch::PAGE_OFFSET_MASK);
        if (addr != 0) && round_to_min && (addr < DEFAULT_MMAP_MIN_ADDR) {
            Some(VirtAddr::new(page_align_up(DEFAULT_MMAP_MIN_ADDR)))
        } else if addr == 0 && fixed_hint {
            Some(VirtAddr::new(0))
        } else if addr == 0 {
            None
        } else {
            Some(VirtAddr::new(addr))
        }
    }

    fn reservation_region_for_hint(
        start_vaddr: VirtAddr,
        len: usize,
        round_to_min: bool,
        fixed_hint: bool,
    ) -> Option<VirtRegion> {
        Self::round_mmap_hint(start_vaddr, round_to_min, fixed_hint)
            .map(|start| VirtRegion::new(start, len))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn map_anonymous_wait(
        self: &Arc<Self>,
        start_vaddr: VirtAddr,
        len: usize,
        prot_flags: ProtFlags,
        map_flags: MapFlags,
        round_to_min: bool,
        allocate_at_once: bool,
    ) -> Result<VirtPageFrame, SystemError> {
        let len = page_align_up(len);
        loop {
            let mut guard = self.write();
            let fixed_hint =
                map_flags.intersects(MapFlags::MAP_FIXED | MapFlags::MAP_FIXED_NOREPLACE);
            if let Some(region) =
                Self::reservation_region_for_hint(start_vaddr, len, round_to_min, fixed_hint)
            {
                if guard.mappings.first_reservation_conflict(region).is_some() {
                    drop(guard);
                    self.wait_for_no_reservation_conflict(region);
                    continue;
                }
            }

            let (page, notifications) = match guard.map_anonymous_collect(
                start_vaddr,
                len,
                prot_flags,
                map_flags,
                round_to_min,
                allocate_at_once,
            ) {
                Ok(outcome) => outcome,
                Err(failure) => {
                    drop(guard);
                    InnerAddressSpace::notify_close_notifications(failure.notifications);
                    return Err(failure.err);
                }
            };
            drop(guard);
            InnerAddressSpace::notify_close_notifications(notifications);
            return Ok(page);
        }
    }

    pub fn has_vma_intersection(
        self: &Arc<Self>,
        start_vaddr: VirtAddr,
        len: usize,
    ) -> Result<bool, SystemError> {
        let end = start_vaddr
            .data()
            .checked_add(len)
            .ok_or(SystemError::EINVAL)?;
        if len == 0 || end > MMArch::USER_END_VADDR.data() {
            return Err(SystemError::EINVAL);
        }

        let requested = VirtRegion::new(start_vaddr, len);
        let guard = self.read();
        Ok(guard.mappings.has_conflict(requested))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn file_mapping(
        self: &Arc<Self>,
        start_vaddr: VirtAddr,
        len: usize,
        prot_flags: ProtFlags,
        map_flags: MapFlags,
        fd: i32,
        offset: usize,
        round_to_min: bool,
        allocate_at_once: bool,
    ) -> Result<VirtPageFrame, SystemError> {
        let binding = ProcessManager::current_pcb().fd_table();
        let fd_table_guard = binding.read();
        let file = fd_table_guard
            .get_file_by_fd(fd)
            .ok_or(SystemError::EBADF)?;
        drop(fd_table_guard);

        self.file_mapping_with_file(
            file,
            start_vaddr,
            len,
            prot_flags,
            map_flags,
            offset,
            round_to_min,
            allocate_at_once,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn file_mapping_with_file(
        self: &Arc<Self>,
        file: Arc<File>,
        start_vaddr: VirtAddr,
        len: usize,
        prot_flags: ProtFlags,
        map_flags: MapFlags,
        offset: usize,
        round_to_min: bool,
        allocate_at_once: bool,
    ) -> Result<VirtPageFrame, SystemError> {
        self.file_mapping_with_file_ext(FileMappingWithFileArgs {
            file,
            start_vaddr,
            len,
            prot_flags,
            map_flags,
            may_exec: true,
            offset,
            round_to_min,
            allocate_at_once,
            sysv_shm: None,
            fixed_noreplace_conflict_error_before_mmap_min: None,
        })
    }

    pub fn file_mapping_with_file_ext(
        self: &Arc<Self>,
        args: FileMappingWithFileArgs,
    ) -> Result<VirtPageFrame, SystemError> {
        let FileMappingWithFileArgs {
            file,
            start_vaddr,
            len,
            prot_flags,
            map_flags,
            may_exec,
            offset,
            round_to_min,
            allocate_at_once,
            sysv_shm,
            fixed_noreplace_conflict_error_before_mmap_min,
        } = args;
        let len = page_align_up(len);
        if len == 0 {
            return Err(SystemError::EINVAL);
        }

        let _force_lazy_on_page_fault_arch = allocate_at_once && MMArch::PAGE_FAULT_ENABLED;

        let file_mode = file.mode();
        if file_mode.contains(FileMode::FMODE_PATH) {
            return Err(SystemError::EBADF);
        }

        let wants_access = prot_flags != ProtFlags::PROT_NONE;
        if wants_access && !file_mode.contains(FileMode::FMODE_READ) {
            return Err(SystemError::EACCES);
        }
        if prot_flags.contains(ProtFlags::PROT_EXEC) && !file_mode.contains(FileMode::FMODE_READ) {
            return Err(SystemError::EACCES);
        }
        if prot_flags.contains(ProtFlags::PROT_WRITE) {
            if map_flags.contains(MapFlags::MAP_SHARED) {
                if !file_mode.contains(FileMode::FMODE_WRITE) {
                    return Err(SystemError::EACCES);
                }
            } else if !file_mode.contains(FileMode::FMODE_READ) {
                return Err(SystemError::EACCES);
            }
        }

        if matches!(file.file_type(), FileType::Pipe | FileType::Dir) {
            return Err(SystemError::ENODEV);
        }
        if (offset & (MMArch::PAGE_SIZE - 1)) != 0 {
            return Err(SystemError::EINVAL);
        }

        let pgoff = offset >> MMArch::PAGE_SHIFT;
        let page_count = PageFrameCount::from_bytes(len).unwrap();
        let may_write =
            !map_flags.contains(MapFlags::MAP_SHARED) || file_mode.contains(FileMode::FMODE_WRITE);
        let vma_file = file.inode().mmap_effective_file(&file)?;

        loop {
            let mut guard = self.write();
            let fixed_hint =
                map_flags.intersects(MapFlags::MAP_FIXED | MapFlags::MAP_FIXED_NOREPLACE);
            let mut close_notifications = VmaCloseNotifications::default();
            macro_rules! map_fail {
                ($err:expr) => {{
                    drop(guard);
                    InnerAddressSpace::notify_close_notifications(close_notifications);
                    return Err($err);
                }};
            }
            if let Some(conflict_error) = fixed_noreplace_conflict_error_before_mmap_min.as_ref() {
                if map_flags.contains(MapFlags::MAP_FIXED_NOREPLACE) {
                    let end = start_vaddr
                        .data()
                        .checked_add(len)
                        .ok_or(SystemError::EINVAL)?;
                    if end > MMArch::USER_END_VADDR.data()
                        || !start_vaddr.check_aligned(MMArch::PAGE_SIZE)
                    {
                        map_fail!(SystemError::EINVAL);
                    }
                    let requested = VirtRegion::new(start_vaddr, len);
                    if guard
                        .mappings
                        .first_reservation_conflict(requested)
                        .is_some()
                    {
                        drop(guard);
                        self.wait_for_no_reservation_conflict(requested);
                        continue;
                    }
                    if guard.mappings.has_conflict(requested) {
                        map_fail!(conflict_error.clone());
                    }
                }
            }
            let page = match Self::round_mmap_hint(start_vaddr, round_to_min, fixed_hint) {
                Some(vaddr) => {
                    let mmap_min = guard.mmap_min;
                    match guard.find_free_at_prepare(mmap_min, vaddr, len, map_flags) {
                        Ok(region) => VirtPageFrame::new(region.start()),
                        Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => {
                            let region = VirtRegion::new(vaddr, len);
                            drop(guard);
                            self.wait_for_no_reservation_conflict(region);
                            continue;
                        }
                        Err(err) => return Err(err),
                    }
                }
                None => {
                    let region = guard
                        .mappings
                        .find_free(guard.mmap_min, len)
                        .ok_or(SystemError::ENOMEM)?;
                    VirtPageFrame::new(region.start())
                }
            };
            let region = VirtRegion::new(page.virt_address(), len);

            let mut vm_flags = VmFlags::from(prot_flags)
                | VmFlags::from(map_flags)
                | guard.mlock_future
                | VmFlags::VM_MAYREAD
                | VmFlags::VM_NONE;
            if may_exec {
                vm_flags |= VmFlags::VM_MAYEXEC;
            }
            if may_write {
                vm_flags |= VmFlags::VM_MAYWRITE;
            }

            if vm_flags.contains(VmFlags::VM_LOCKED) {
                let error = if map_flags.contains(MapFlags::MAP_LOCKED)
                    && !InnerAddressSpace::has_mlock_quota()
                {
                    SystemError::EPERM
                } else {
                    SystemError::EAGAIN_OR_EWOULDBLOCK
                };
                if let Err(err) = guard.check_mlock_rlimit_for_pages(page_count.data(), error) {
                    map_fail!(err);
                }
            }
            if let Err(err) = guard.check_rlimit_as_for_region(region, len, map_flags) {
                map_fail!(err);
            }

            if let Err(err) = file.inode().check_mmap_file(&file, len, offset, vm_flags) {
                map_fail!(err);
            }

            if map_flags.contains(MapFlags::MAP_FIXED) && guard.mappings.has_conflict(region) {
                match guard.munmap_collect(
                    VirtPageFrame::new(region.start()),
                    PageFrameCount::from_bytes(region.size()).unwrap(),
                ) {
                    Ok(notifications) => close_notifications.extend(notifications),
                    Err(err) => map_fail!(err),
                }
            }

            let reservation_id = match guard.mappings.reserve_region(region) {
                Ok(reservation_id) => reservation_id,
                Err(err) => map_fail!(err),
            };
            let entry_flags = EntryFlags::from_prot_flags(prot_flags, true);
            let locked_pages_reserved = if vm_flags.contains(VmFlags::VM_LOCKED) {
                let new_locked_vm = match guard.locked_vm.checked_add(page_count.data()) {
                    Some(new_locked_vm) => new_locked_vm,
                    None => {
                        if guard.mappings.cancel_reservation(reservation_id).is_some() {
                            drop(guard);
                            self.wake_reservation_waiters();
                        } else {
                            drop(guard);
                        }
                        InnerAddressSpace::notify_close_notifications(close_notifications);
                        return Err(SystemError::ENOMEM);
                    }
                };
                guard.locked_vm = new_locked_vm;
                true
            } else {
                false
            };
            let lazy_vma = if MMArch::PAGE_FAULT_ENABLED {
                let vma = LockedVMA::new(VMA::new(
                    region,
                    vm_flags,
                    entry_flags,
                    Some(vma_file.clone()),
                    Some(pgoff),
                    false,
                ));
                if let Some(sysv_shm) = sysv_shm.clone() {
                    vma.lock().set_sysv_shm(Some(sysv_shm));
                }
                Some(vma)
            } else {
                None
            };
            drop(guard);
            InnerAddressSpace::notify_close_notifications(close_notifications);

            let mut reservation = MmapReservationGuard::new(self.clone(), reservation_id);
            let hook_result =
                file.inode()
                    .mmap_file(&file, region.start().data(), len, offset, vm_flags);
            let file_mmap_opened = hook_result.is_ok();
            let mut guard = self.write();
            macro_rules! close_file_mmap_if_opened {
                () => {
                    if file_mmap_opened {
                        InnerAddressSpace::notify_vma_close(VmaCloseNotification {
                            file: file.clone(),
                            region,
                            vm_flags,
                        });
                    }
                };
            }
            macro_rules! release_locked_pages_if_reserved {
                () => {
                    if locked_pages_reserved {
                        guard.locked_vm =
                            guard.locked_vm.checked_sub(page_count.data()).unwrap_or_else(|| {
                                error!(
                                    "file mmap locked_vm accounting underflow: locked_vm={}, pages={}",
                                    guard.locked_vm,
                                    page_count.data()
                                );
                                0
                            });
                    }
                };
            }
            macro_rules! cancel_reservation_and_unlock_pages {
                () => {{
                    release_locked_pages_if_reserved!();
                    if guard.mappings.cancel_reservation(reservation_id).is_some() {
                        drop(guard);
                        reservation.disarm();
                        self.wake_reservation_waiters();
                    } else {
                        drop(guard);
                        reservation.disarm();
                    }
                }};
            }

            if let Err(err) = hook_result {
                if err != SystemError::ENOSYS {
                    cancel_reservation_and_unlock_pages!();
                    return Err(err);
                }
            }

            let new_vma = if let Some(vma) = lazy_vma {
                vma
            } else {
                let mut flusher = crate::mm::page::DeferredFlusher::new();
                compiler_fence(Ordering::SeqCst);
                let _pt_edit = self.page_table_edit();
                match VMA::zeroed(
                    page,
                    page_count,
                    vm_flags,
                    entry_flags,
                    &mut guard.user_mapper.utable,
                    &mut flusher,
                    Some(vma_file.clone()),
                    Some(pgoff),
                ) {
                    Ok(vma) => {
                        if let Some(sysv_shm) = sysv_shm.clone() {
                            vma.lock().set_sysv_shm(Some(sysv_shm));
                        }
                        vma
                    }
                    Err(err) => {
                        cancel_reservation_and_unlock_pages!();
                        close_file_mmap_if_opened!();
                        return Err(err);
                    }
                }
            };

            let sysv_opened = if let Some(sysv_shm) = sysv_shm.as_ref() {
                if let Err(err) = sysv_shm.open_vma() {
                    cancel_reservation_and_unlock_pages!();
                    close_file_mmap_if_opened!();
                    return Err(err);
                }
                true
            } else {
                false
            };

            let new_present_pages = if new_vma.mapped() {
                page_count.data()
            } else {
                0
            };

            if let Err(err) = guard.mappings.commit_reserved_vma(reservation_id, new_vma) {
                let sysv_to_close = if sysv_opened { sysv_shm.clone() } else { None };
                release_locked_pages_if_reserved!();
                drop(guard);
                close_file_mmap_if_opened!();
                if let Some(sysv_shm) = sysv_to_close {
                    sysv_shm.close_vma();
                }
                return Err(err);
            }

            self.account_present_pages_add(new_present_pages);
            reservation.disarm();
            drop(guard);
            self.wake_reservation_waiters();
            return Ok(page);
        }
    }

    pub fn munmap_wait(
        self: &Arc<Self>,
        start_page: VirtPageFrame,
        page_count: PageFrameCount,
    ) -> Result<(), SystemError> {
        let region = VirtRegion::new(start_page.virt_address(), page_count.bytes());
        loop {
            let mut guard = self.write();
            if guard.mappings.first_reservation_conflict(region).is_some() {
                drop(guard);
                self.wait_for_no_reservation_conflict(region);
                continue;
            }
            let notifications = guard.munmap_collect(start_page, page_count)?;
            drop(guard);
            InnerAddressSpace::notify_close_notifications(notifications);
            return Ok(());
        }
    }

    pub fn detach_sysv_shm_wait(self: &Arc<Self>, addr: VirtAddr) -> Result<(), SystemError> {
        let notifications = {
            let mut guard = self.write_guard_no_reservations();
            guard.detach_sysv_shm(addr)?
        };
        InnerAddressSpace::notify_close_notifications(notifications);
        Ok(())
    }

    pub fn mprotect_wait(
        self: &Arc<Self>,
        start_page: VirtPageFrame,
        page_count: PageFrameCount,
        prot_flags: ProtFlags,
    ) -> Result<(), SystemError> {
        let region = VirtRegion::new(start_page.virt_address(), page_count.bytes());
        loop {
            let mut guard = self.write();
            if guard.mappings.first_reservation_conflict(region).is_some() {
                drop(guard);
                self.wait_for_no_reservation_conflict(region);
                continue;
            }
            return guard.mprotect(start_page, page_count, prot_flags);
        }
    }

    pub fn madvise_wait(
        self: &Arc<Self>,
        start_page: VirtPageFrame,
        page_count: PageFrameCount,
        behavior: MadvFlags,
    ) -> Result<(), SystemError> {
        let region = VirtRegion::new(start_page.virt_address(), page_count.bytes());
        loop {
            let mut guard = self.write();
            if guard.mappings.first_reservation_conflict(region).is_some() {
                drop(guard);
                self.wait_for_no_reservation_conflict(region);
                continue;
            }
            return guard.madvise(start_page, page_count, behavior);
        }
    }

    pub fn mincore_wait(
        self: &Arc<Self>,
        start_page: VirtPageFrame,
        page_count: PageFrameCount,
        vec: &mut [u8],
    ) -> Result<(), SystemError> {
        let region = VirtRegion::new(start_page.virt_address(), page_count.bytes());
        loop {
            let guard = self.read();
            if guard.mappings.first_reservation_conflict(region).is_some() {
                drop(guard);
                self.wait_for_no_reservation_conflict(region);
                continue;
            }
            return guard.mincore(start_page, page_count, vec);
        }
    }

    pub fn mremap_wait(
        self: &Arc<Self>,
        old_vaddr: VirtAddr,
        old_len: usize,
        new_len: usize,
        mremap_flags: MremapFlags,
        new_vaddr: VirtAddr,
        vm_flags: VmFlags,
    ) -> Result<VirtAddr, SystemError> {
        loop {
            let mut guard = self.write();
            let mut wait_region = None;
            if old_len != 0 && old_vaddr.data().checked_add(old_len).is_some() {
                let old_region = VirtRegion::new(old_vaddr, old_len);
                if guard
                    .mappings
                    .first_reservation_conflict(old_region)
                    .is_some()
                {
                    wait_region = Some(old_region);
                } else if new_len > old_len {
                    if let Some(grow_start) = old_vaddr.data().checked_add(old_len) {
                        let grow_region =
                            VirtRegion::new(VirtAddr::new(grow_start), new_len - old_len);
                        if guard
                            .mappings
                            .first_reservation_conflict(grow_region)
                            .is_some()
                        {
                            wait_region = Some(grow_region);
                        }
                    }
                }
            }
            if wait_region.is_none() && mremap_flags.contains(MremapFlags::MREMAP_FIXED) {
                let new_region = VirtRegion::new(new_vaddr, new_len);
                if guard
                    .mappings
                    .first_reservation_conflict(new_region)
                    .is_some()
                {
                    wait_region = Some(new_region);
                }
            }
            if wait_region.is_none()
                && mremap_flags.contains(MremapFlags::MREMAP_DONTUNMAP)
                && !mremap_flags.contains(MremapFlags::MREMAP_FIXED)
                && new_vaddr != VirtAddr::new(0)
                && new_vaddr
                    .data()
                    .checked_add(new_len)
                    .is_some_and(|end| end <= MMArch::USER_END_VADDR.data())
            {
                let new_region = VirtRegion::new(new_vaddr, new_len);
                if guard
                    .mappings
                    .first_reservation_conflict(new_region)
                    .is_some()
                {
                    wait_region = Some(new_region);
                }
            }

            if let Some(region) = wait_region {
                drop(guard);
                self.wait_for_no_reservation_conflict(region);
                continue;
            }

            match guard.mremap(
                old_vaddr,
                old_len,
                new_len,
                mremap_flags,
                new_vaddr,
                vm_flags,
            ) {
                Ok(outcome) => {
                    drop(guard);
                    InnerAddressSpace::notify_close_notifications(outcome.notifications);
                    return Ok(outcome.addr);
                }
                Err(failure) if failure.err == SystemError::EAGAIN_OR_EWOULDBLOCK => {
                    let retry_region = if mremap_flags.contains(MremapFlags::MREMAP_FIXED)
                        || (mremap_flags.contains(MremapFlags::MREMAP_DONTUNMAP)
                            && new_vaddr != VirtAddr::new(0))
                    {
                        VirtRegion::new(new_vaddr, new_len)
                    } else if new_len > old_len {
                        if let Some(grow_start) = old_vaddr.data().checked_add(old_len) {
                            VirtRegion::new(VirtAddr::new(grow_start), new_len - old_len)
                        } else {
                            VirtRegion::new(old_vaddr, old_len.max(MMArch::PAGE_SIZE))
                        }
                    } else {
                        VirtRegion::new(old_vaddr, old_len.max(MMArch::PAGE_SIZE))
                    };
                    if guard
                        .mappings
                        .first_reservation_conflict(retry_region)
                        .is_some()
                    {
                        drop(guard);
                        InnerAddressSpace::notify_close_notifications(failure.notifications);
                        self.wait_for_no_reservation_conflict(retry_region);
                        continue;
                    }
                    drop(guard);
                    InnerAddressSpace::notify_close_notifications(failure.notifications);
                    return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                }
                Err(failure) => {
                    drop(guard);
                    InnerAddressSpace::notify_close_notifications(failure.notifications);
                    return Err(failure.err);
                }
            }
        }
    }

    pub fn set_brk_wait(self: &Arc<Self>, new_addr: VirtAddr) -> Result<usize, SystemError> {
        loop {
            let mut guard = self.write();

            if new_addr < guard.brk_start || new_addr >= MMArch::USER_END_VADDR {
                return Ok(guard.brk.data());
            }
            if new_addr == guard.brk {
                return Ok(guard.brk.data());
            }

            let new_brk = VirtAddr::new(page_align_up(new_addr.data()));
            let wait_region = if new_brk > guard.brk {
                Some(VirtRegion::new(guard.brk, new_brk - guard.brk))
            } else if new_brk < guard.brk {
                Some(VirtRegion::new(new_brk, guard.brk - new_brk))
            } else {
                None
            };
            if let Some(region) = wait_region {
                if guard.mappings.first_reservation_conflict(region).is_some() {
                    drop(guard);
                    self.wait_for_no_reservation_conflict(region);
                    continue;
                }
            }

            unsafe {
                guard.set_brk(new_brk).ok();
                return Ok(guard.sbrk(0).unwrap().data());
            }
        }
    }

    pub fn sbrk_wait(self: &Arc<Self>, incr: isize) -> Result<VirtAddr, SystemError> {
        loop {
            let mut guard = self.write();
            if incr == 0 {
                return Ok(guard.brk);
            }

            let requested = if incr > 0 {
                guard.brk + incr as usize
            } else {
                guard.brk - incr.unsigned_abs()
            };
            let new_brk = VirtAddr::new(page_align_up(requested.data()));
            let wait_region = if new_brk > guard.brk {
                Some(VirtRegion::new(guard.brk, new_brk - guard.brk))
            } else if new_brk < guard.brk {
                Some(VirtRegion::new(new_brk, guard.brk - new_brk))
            } else {
                None
            };

            if let Some(region) = wait_region {
                if guard.mappings.first_reservation_conflict(region).is_some() {
                    drop(guard);
                    self.wait_for_no_reservation_conflict(region);
                    continue;
                }
            }

            return unsafe { guard.sbrk(incr) };
        }
    }

    pub fn try_clone_wait(self: &Arc<Self>) -> Result<Arc<AddressSpace>, SystemError> {
        loop {
            let mut guard = self.write();
            if let Some(region) = guard.mappings.first_reservation_region() {
                drop(guard);
                self.wait_for_no_reservation_conflict(region);
                continue;
            }
            return guard.try_clone();
        }
    }
}

impl Drop for AddressSpace {
    fn drop(&mut self) {
        // Assert that no CPUs still reference this mm when it is dropped in debug builds.
        // In release builds, degrade gracefully to avoid false positives (a leak is better than a panic).
        #[cfg(debug_assertions)]
        {
            let g = self.active_cpus.lock();
            debug_assert!(
                g.is_empty(),
                "AddressSpace dropped with non-empty active_cpus; id={}",
                self.id
            );
        }
    }
}

impl core::ops::Deref for AddressSpace {
    type Target = RwSem<InnerAddressSpace>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl core::ops::DerefMut for AddressSpace {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

struct MmapReservationGuard {
    mm: Arc<AddressSpace>,
    id: MmapReservationId,
    active: bool,
}

impl MmapReservationGuard {
    fn new(mm: Arc<AddressSpace>, id: MmapReservationId) -> Self {
        Self {
            mm,
            id,
            active: true,
        }
    }

    fn disarm(&mut self) {
        self.active = false;
    }
}

impl Drop for MmapReservationGuard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let mut guard = self.mm.write();
        if guard.mappings.cancel_reservation(self.id).is_some() {
            drop(guard);
            self.mm.wake_reservation_waiters();
        }
    }
}

/// @brief 用户地址空间结构体（每个进程都有一个）
#[derive(Debug)]
pub struct InnerAddressSpace {
    mm_id: u64,
    pub user_mapper: UserMapper,
    pub mappings: UserMappings,
    /// 已锁定的用户页数量，以页为单位。
    pub locked_vm: usize,
    /// Flags inherited by future mappings after mlockall(MCL_FUTURE).
    pub mlock_future: VmFlags,
    pub mmap_min: VirtAddr,
    /// 用户栈信息结构体
    pub user_stack: Option<UserStack>,

    pub elf_brk_start: VirtAddr,
    pub elf_brk: VirtAddr,

    /// 当前进程的堆空间的起始地址
    pub brk_start: VirtAddr,
    /// 当前进程的堆空间的结束地址(不包含)
    pub brk: VirtAddr,

    pub start_code: VirtAddr,
    pub end_code: VirtAddr,
    pub start_data: VirtAddr,
    pub end_data: VirtAddr,

    /// Weak reference back to the outer `AddressSpace`.
    ///
    /// Back-filled by `AddressSpace::new` after constructing the Arc; used by internal
    /// `munmap`/`mprotect`/... to obtain `Arc<AddressSpace>` for constructing `MmuGather`
    /// and initiating TLB shootdown.
    outer: Weak<AddressSpace>,
}

struct VmaCloseNotification {
    file: Arc<File>,
    region: VirtRegion,
    vm_flags: VmFlags,
}

#[derive(Default)]
struct VmaCloseNotifications {
    vma: Vec<VmaCloseNotification>,
    sysv: Vec<Arc<SysVShmAttach>>,
}

impl VmaCloseNotifications {
    fn is_empty(&self) -> bool {
        self.vma.is_empty() && self.sysv.is_empty()
    }

    fn extend(&mut self, mut other: VmaCloseNotifications) {
        self.vma.append(&mut other.vma);
        self.sysv.append(&mut other.sysv);
    }
}

struct MremapOutcome {
    addr: VirtAddr,
    notifications: VmaCloseNotifications,
}

struct MremapFailure {
    err: SystemError,
    notifications: VmaCloseNotifications,
}

impl From<SystemError> for MremapFailure {
    fn from(err: SystemError) -> Self {
        Self {
            err,
            notifications: VmaCloseNotifications::default(),
        }
    }
}

struct MmapFailure {
    err: SystemError,
    notifications: VmaCloseNotifications,
}

impl From<SystemError> for MmapFailure {
    fn from(err: SystemError) -> Self {
        Self {
            err,
            notifications: VmaCloseNotifications::default(),
        }
    }
}

struct MunmapVmaPlan {
    original_region: VirtRegion,
    intersection: VirtRegion,
    locked_vm_after_unmap: Option<usize>,
    split_lifecycle: VmaSplitLifecycle,
}

struct MprotectVmaPlan {
    original_region: VirtRegion,
    intersection: VirtRegion,
    new_vm_flags: VmFlags,
    split_lifecycle: VmaSplitLifecycle,
}

struct MadviseVmaPlan {
    original_region: VirtRegion,
    intersection: VirtRegion,
    split_lifecycle: VmaSplitLifecycle,
}

#[derive(Debug)]
struct VmaSplitLifecycle {
    sysv_shm: Option<Arc<SysVShmAttach>>,
    open_count: usize,
    committed: bool,
}

impl VmaSplitLifecycle {
    fn none() -> Self {
        Self {
            sysv_shm: None,
            open_count: 0,
            committed: false,
        }
    }

    fn commit(mut self) {
        self.committed = true;
    }

    fn rollback_into(mut self, notifications: &mut VmaCloseNotifications) {
        if self.committed {
            return;
        }
        if let Some(sysv_shm) = self.sysv_shm.take() {
            for _ in 0..self.open_count {
                notifications.sysv.push(sysv_shm.clone());
            }
        }
        self.open_count = 0;
        self.committed = true;
    }
}

impl Drop for VmaSplitLifecycle {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        error!(
            "VmaSplitLifecycle dropped without explicit commit/rollback; falling back to immediate SysV SHM close"
        );
        if let Some(sysv_shm) = self.sysv_shm.as_ref() {
            for _ in 0..self.open_count {
                sysv_shm.close_vma();
            }
        }
    }
}

impl InnerAddressSpace {
    /// 当前地址空间已占用的虚拟内存字节数（简单求和所有 VMA 尺寸）
    pub fn vma_usage_bytes(&self) -> usize {
        let vma_bytes = self
            .mappings
            .iter_vmas()
            .map(|v| {
                let g = v.lock();
                g.region().size()
            })
            .sum::<usize>();
        vma_bytes.saturating_add(self.mappings.reservation_usage_bytes())
    }

    pub fn new(_create_stack: bool) -> Result<Self, SystemError> {
        let result = Self {
            mm_id: 0,
            user_mapper: MMArch::setup_new_usermapper()?,
            mappings: UserMappings::new(),
            locked_vm: 0,
            mlock_future: VmFlags::VM_NONE,
            mmap_min: VirtAddr(DEFAULT_MMAP_MIN_ADDR),
            elf_brk_start: VirtAddr::new(0),
            elf_brk: VirtAddr::new(0),
            brk_start: MMArch::USER_BRK_START,
            brk: MMArch::USER_BRK_START,
            user_stack: None,
            start_code: VirtAddr(0),
            end_code: VirtAddr(0),
            start_data: VirtAddr(0),
            end_data: VirtAddr(0),
            outer: Weak::new(),
        };

        return Ok(result);
    }

    /// 尝试克隆当前进程的地址空间，包括这些映射都会被克隆
    ///
    /// # Returns
    ///
    /// 返回克隆后的，新的地址空间的Arc指针
    #[inline(never)]
    pub fn try_clone(&mut self) -> Result<Arc<AddressSpace>, SystemError> {
        if self.mappings.first_reservation_region().is_some() {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }

        let new_addr_space = AddressSpace::new(false)?;
        let mut new_guard = new_addr_space.write();

        // 父 mm 可能是多线程共享的 mm（CLONE_VM / CLONE_THREAD），此时正在其他 CPU 上跑的线程
        // 也可能在父页表上缓存了 writable 的 TLB。下面 COW 写保护父进程 PTE 时，
        // 必须走 mm-aware shootdown：只做本地 invlpg 的话，远端 CPU 仍可能用旧的可写 TLB 写进去，
        // 破坏 COW 语义（风险 4 残留）。这里用 MmuGather 累积整个被改写的范围，
        // 循环结束后由 tlb.finish() 统一触发 flush_tlb_mm_range，对父 mm 的所有活跃 CPU 同步 shootdown。
        let parent_mm = self
            .outer
            .upgrade()
            .expect("InnerAddressSpace::try_clone called before AddressSpace::new finished");
        let mut parent_tlb = MmuGather::gather(&parent_mm);

        // 仅拷贝用户栈的结构体信息（元数据），实际的用户栈页面内容会在下面的 VMA 循环中处理
        unsafe {
            new_guard.user_stack = Some(self.user_stack.as_ref().unwrap().clone_info_only());
        }

        // 拷贝空洞
        new_guard.mappings.vm_holes = self.mappings.vm_holes.clone();

        // 拷贝其他地址空间属性
        new_guard.brk = self.brk;
        new_guard.brk_start = self.brk_start;
        new_guard.mmap_min = self.mmap_min;
        new_guard.elf_brk = self.elf_brk;
        new_guard.elf_brk_start = self.elf_brk_start;
        new_guard.start_code = self.start_code;
        new_guard.end_code = self.end_code;
        new_guard.start_data = self.start_data;
        new_guard.end_data = self.end_data;

        let mut parent_cow_remaps: Vec<(VirtAddr, EntryFlags<MMArch>)> = Vec::new();
        let mut child_present_pages = 0usize;
        let clone_result: Result<(), SystemError> = (|| {
            // 遍历父进程的每个VMA，根据VMA属性进行适当的复制
            // 参考 Linux: https://code.dragonos.org.cn/xref/linux-6.6.21/mm/memory.c#copy_page_range
            for vma in self.mappings.vmas.iter() {
                // 锁顺序：VMA 锁 -> page_manager -> shm_manager，避免交叉获取导致死锁。
                let vma_guard = vma.lock();

                // VM_DONTCOPY: 跳过不复制的VMA (例如 MADV_DONTFORK 标记的)
                if vma_guard.vm_flags().contains(VmFlags::VM_DONTCOPY) {
                    drop(vma_guard);
                    continue;
                }

                let vm_flags = *vma_guard.vm_flags();
                let is_shared = vm_flags.contains(VmFlags::VM_SHARED);
                let region = *vma_guard.region();
                let page_flags = vma_guard.flags();
                let sysv_shm = vma_guard.sysv_shm();

                // 创建新的VMA
                let mut child_vma = vma_guard.clone_info_only();
                child_vma.vm_flags &= VmFlags::VM_LOCKED_CLEAR_MASK;
                let new_vma = LockedVMA::new(child_vma);
                new_guard.mappings.insert_vma(new_vma.clone());
                drop(vma_guard);

                if let Some(sysv_shm) = sysv_shm {
                    if let Err(err) = sysv_shm.open_vma() {
                        if let Some(removed) = new_guard.mappings.remove_vma(&region) {
                            removed.lock().set_mapped(false);
                        }
                        return Err(err);
                    }
                }

                // 根据VMA类型进行不同的页面复制策略
                let start_page = region.start();
                let end_page = region.end();
                let mut current_page = start_page;

                {
                    let _parent_pt_edit = parent_mm.page_table_edit();
                    let old_mapper = &mut self.user_mapper.utable;
                    let new_mapper = &mut new_guard.user_mapper.utable;
                    let new_vma_mlocked = new_vma.lock().vm_flags().contains(VmFlags::VM_LOCKED);
                    let mut page_manager_guard = page_manager_lock();

                    while current_page < end_page {
                        if let Some((phys_addr, old_flags)) = old_mapper.translate(current_page) {
                            unsafe {
                                if is_shared {
                                    if new_mapper
                                        .map_phys(current_page, phys_addr, page_flags)
                                        .is_none()
                                    {
                                        return Err(SystemError::ENOMEM);
                                    }
                                } else {
                                    let cow_flags = page_flags.set_write(false);

                                    if old_flags.has_write() {
                                        if let Some(flush) =
                                            old_mapper.remap(current_page, cow_flags)
                                        {
                                            flush.ignore();
                                            parent_tlb.accumulate_range(current_page);
                                            parent_cow_remaps.push((current_page, old_flags));
                                        }
                                    }

                                    if new_mapper
                                        .map_phys(current_page, phys_addr, cow_flags)
                                        .is_none()
                                    {
                                        return Err(SystemError::ENOMEM);
                                    }
                                }
                                if let Some(page) = page_manager_guard.get(&phys_addr) {
                                    page.write().insert_vma(new_vma.clone(), new_vma_mlocked);
                                }
                                child_present_pages += 1;
                            }
                        }
                        current_page = VirtAddr::new(current_page.data() + MMArch::PAGE_SIZE);
                    }
                }
            }
            Ok(())
        })();

        if let Err(err) = clone_result {
            {
                let _parent_pt_edit = parent_mm.page_table_edit();
                let old_mapper = &mut self.user_mapper.utable;
                for (page, flags) in parent_cow_remaps.into_iter().rev() {
                    if let Some(flush) = unsafe { old_mapper.remap(page, flags) } {
                        unsafe { flush.ignore() };
                        parent_tlb.accumulate_range(page);
                    } else {
                        warn!("fork rollback lost expected parent PTE at {:?}", page);
                    }
                }
            }
            drop(new_guard);
            parent_tlb.finish();
            return Err(err);
        }

        drop(new_guard);
        new_addr_space.account_present_pages_add(child_present_pages);
        // 完成父 mm 的 mm-aware shootdown：INV-3 要求 TLB 生效完成后再继续后续逻辑，
        // 此处没有 page 进入 pending_pages，因此实际只触发 flush_tlb_mm_range。
        parent_tlb.finish();
        return Ok(new_addr_space);
    }

    /// Check if the stack can be extended
    pub fn can_extend_stack(&self, bytes: usize) -> bool {
        let bytes = page_align_up(bytes);
        let stack = self.user_stack.as_ref().unwrap();
        let new_size = stack.mapped_size + bytes;
        if new_size > stack.max_limit {
            // Don't exceed the maximum stack size
            return false;
        }
        return true;
    }

    /// 拓展用户栈
    /// ## 参数
    ///
    /// - `bytes`: 拓展大小
    pub fn extend_stack(&mut self, mut bytes: usize) -> Result<(), SystemError> {
        // log::debug!("extend user stack");

        // Layout
        // -------------- high->sp
        // | stack pages|
        // |------------|
        // | stack pages|
        // |------------|
        // | not mapped |
        // -------------- low

        let prot_flags = ProtFlags::PROT_READ | ProtFlags::PROT_WRITE | ProtFlags::PROT_EXEC;
        let map_flags = MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS | MapFlags::MAP_GROWSDOWN;
        let stack = self.user_stack.as_mut().unwrap();

        bytes = page_align_up(bytes);
        stack.mapped_size += bytes;
        // map new stack pages
        let extend_stack_start = stack.stack_bottom - stack.mapped_size;

        self.map_anonymous(
            extend_stack_start,
            bytes,
            prot_flags,
            map_flags,
            false,
            false,
        )?;
        return Ok(());
    }

    /// 判断当前的地址空间是否是当前进程的地址空间
    #[inline]
    pub fn is_current(&self) -> bool {
        return self.user_mapper.utable.is_current();
    }

    /// Obtain the outer `Arc<AddressSpace>`.
    ///
    /// Back-filled during `AddressSpace::new` construction; upgrade should always succeed on normal paths.
    /// Returns `None` only in extreme scenarios (Weak not yet assigned / mm already destroyed).
    #[inline]
    pub fn outer_addr_space(&self) -> Option<Arc<AddressSpace>> {
        self.outer.upgrade()
    }

    fn has_mlock_quota() -> bool {
        let pcb = ProcessManager::current_pcb();
        pcb.get_rlimit(RLimitID::Memlock).rlim_cur != 0
            || pcb.cred().has_capability(CAPFlags::CAP_IPC_LOCK)
    }

    fn check_mlock_rlimit_for_pages(
        &self,
        new_pages: usize,
        error: SystemError,
    ) -> Result<(), SystemError> {
        let pcb = ProcessManager::current_pcb();
        if pcb.cred().has_capability(CAPFlags::CAP_IPC_LOCK) {
            return Ok(());
        }

        let total_pages = self.locked_vm.checked_add(new_pages).ok_or(error.clone())?;
        let total_bytes = (total_pages as u128) * (MMArch::PAGE_SIZE as u128);
        let rlimit = pcb.get_rlimit(RLimitID::Memlock).rlim_cur;
        if total_bytes > rlimit as u128 {
            return Err(error);
        }

        Ok(())
    }

    fn check_rlimit_as_for_bytes(&self, len: usize) -> Result<(), SystemError> {
        self.check_rlimit_as_for_growth(len)
    }

    fn check_rlimit_as_for_region(
        &self,
        region: VirtRegion,
        len: usize,
        map_flags: MapFlags,
    ) -> Result<(), SystemError> {
        let covered = if map_flags.contains(MapFlags::MAP_FIXED) {
            self.covered_vma_bytes(region)
        } else {
            0
        };
        self.check_rlimit_as_for_growth(len.saturating_sub(covered))
    }

    fn check_rlimit_as_for_growth(&self, growth: usize) -> Result<(), SystemError> {
        if growth == 0 {
            return Ok(());
        }

        if !ProcessManager::initialized() {
            return Ok(());
        }

        let rlim_as = ProcessManager::current_pcb()
            .get_rlimit(RLimitID::As)
            .rlim_cur as usize;
        if rlim_as == usize::MAX {
            return Ok(());
        }

        let limit_pages = rlim_as >> MMArch::PAGE_SHIFT;
        let used_pages = self.vma_usage_bytes() >> MMArch::PAGE_SHIFT;
        let growth_pages = growth
            .checked_add(MMArch::PAGE_SIZE - 1)
            .ok_or(SystemError::ENOMEM)?
            >> MMArch::PAGE_SHIFT;
        if used_pages
            .checked_add(growth_pages)
            .is_none_or(|v| v > limit_pages)
        {
            Err(SystemError::ENOMEM)
        } else {
            Ok(())
        }
    }

    fn covered_vma_bytes(&self, region: VirtRegion) -> usize {
        self.mappings
            .conflicts(region)
            .into_iter()
            .filter_map(|vma| vma.lock().region().intersect(&region))
            .fold(0usize, |total, intersection| {
                total.saturating_add(intersection.size())
            })
    }

    fn mlock_fault_flags(vm_flags: VmFlags) -> Option<FaultFlags> {
        if vm_flags.contains(VmFlags::VM_WRITE) {
            Some(FaultFlags::FAULT_FLAG_WRITE)
        } else if vm_flags.contains(VmFlags::VM_READ) {
            Some(FaultFlags::empty())
        } else if vm_flags.contains(VmFlags::VM_EXEC) {
            Some(FaultFlags::FAULT_FLAG_INSTRUCTION)
        } else {
            None
        }
    }

    fn add_present_page_mlock_ref(&mut self, addr: VirtAddr, vma: &Arc<LockedVMA>) {
        if let Some((paddr, _)) = self.user_mapper.utable.translate(addr) {
            let mut page_manager_guard = page_manager_lock();
            let page = page_manager_guard.get_unwrap(&paddr);
            page.write().add_mlocked_vma_ref(vma);
        }
    }

    fn update_present_page_mlock_refs(
        &mut self,
        vma: &Arc<LockedVMA>,
        start: VirtAddr,
        end: VirtAddr,
        old_locked: bool,
        new_locked: bool,
    ) {
        if old_locked == new_locked {
            return;
        }

        let mut pages_to_reclassify = Vec::new();
        let mut vaddr = start;
        while vaddr < end {
            if let Some((paddr, _)) = self.user_mapper.utable.translate(vaddr) {
                let page = {
                    let mut page_manager_guard = page_manager_lock();
                    page_manager_guard.get_unwrap(&paddr)
                };
                {
                    let mut page_guard = page.write();
                    if new_locked {
                        page_guard.add_mlocked_vma_ref(vma);
                    } else {
                        page_guard.remove_mlocked_vma_ref(vma);
                    }
                }
                if !new_locked {
                    pages_to_reclassify.push(page);
                }
            }
            vaddr = VirtAddr::new(vaddr.data() + MMArch::PAGE_SIZE);
        }

        for page in pages_to_reclassify {
            Self::remove_page_unevictable_if_unneeded(&page);
        }
    }

    fn populate_vma_page(
        &mut self,
        vma: Arc<LockedVMA>,
        addr: VirtAddr,
        fault_flags: FaultFlags,
    ) -> Result<(), SystemError> {
        let mm = self.outer_addr_space().ok_or(SystemError::EFAULT)?;
        let fault = unsafe {
            let message =
                PageFaultMessage::new(vma, addr, fault_flags, &mut self.user_mapper.utable, mm);
            PageFaultHandler::handle_mm_fault(message)
        };

        if fault.contains(VmFaultReason::VM_FAULT_COMPLETED) {
            Ok(())
        } else if fault.contains(VmFaultReason::VM_FAULT_OOM) {
            Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
        } else {
            Err(SystemError::ENOMEM)
        }
    }

    fn populate_vma_range(
        &mut self,
        start: VirtAddr,
        len: usize,
        fault_in_missing: bool,
    ) -> Result<(), SystemError> {
        let target = Self::checked_user_region(start, len)?;
        let mut vmas = self.mappings.conflicts(target);
        vmas.sort_by_key(|vma| vma.lock().region().start().data());

        let mut cursor = target.start();
        for vma in vmas {
            let (region, vm_flags) = {
                let guard = vma.lock();
                (*guard.region(), *guard.vm_flags())
            };
            let Some(intersection) = region.intersect(&target) else {
                continue;
            };
            if intersection.start() > cursor {
                return Err(SystemError::ENOMEM);
            }
            if intersection.end() <= cursor {
                continue;
            }

            let fault_flags = Self::mlock_fault_flags(vm_flags).ok_or(SystemError::ENOMEM)?;
            let mut addr = intersection.start();
            while addr < intersection.end() {
                if self.user_mapper.utable.translate(addr).is_some() {
                    if vm_flags.contains(VmFlags::VM_LOCKED) {
                        self.add_present_page_mlock_ref(addr, &vma);
                    }
                } else if fault_in_missing {
                    self.populate_vma_page(vma.clone(), addr, fault_flags)?;
                }
                addr = VirtAddr::new(addr.data() + MMArch::PAGE_SIZE);
            }

            cursor = intersection.end();
            if cursor >= target.end() {
                break;
            }
        }

        if cursor != target.end() {
            return Err(SystemError::ENOMEM);
        }

        Ok(())
    }

    fn best_effort_locked_population(&mut self, start: VirtAddr, len: usize, vm_flags: VmFlags) {
        if len == 0 || !vm_flags.contains(VmFlags::VM_LOCKED) {
            return;
        }

        let fault_in_missing = !vm_flags.contains(VmFlags::VM_LOCKONFAULT);
        let _ = self.populate_vma_range(start, len, fault_in_missing);
    }

    fn post_map_population(&mut self, start: VirtAddr, len: usize, map_flags: MapFlags) {
        let Some(vma) = self.mappings.contains(start) else {
            return;
        };

        let vm_flags = *vma.lock().vm_flags();
        let fault_in_missing = map_flags.contains(MapFlags::MAP_POPULATE)
            || (vm_flags.contains(VmFlags::VM_LOCKED)
                && !vm_flags.contains(VmFlags::VM_LOCKONFAULT));
        if fault_in_missing || vm_flags.contains(VmFlags::VM_LOCKED) {
            let _ = self.populate_vma_range(start, len, fault_in_missing);
        }
    }

    /// 进行匿名页映射
    ///
    /// ## 参数
    ///
    /// - `start_vaddr`：映射的起始地址
    /// - `len`：映射的长度
    /// - `prot_flags`：保护标志
    /// - `map_flags`：映射标志
    /// - `round_to_min`：是否将`start_vaddr`对齐到`mmap_min`，如果为`true`，则当`start_vaddr`不为0时，会对齐到`mmap_min`，否则仅向下对齐到页边界
    /// - `allocate_at_once`：是否立即分配物理空间
    ///
    /// ## 返回
    ///
    /// 返回映射的起始虚拟页帧
    pub fn map_anonymous(
        &mut self,
        start_vaddr: VirtAddr,
        len: usize,
        prot_flags: ProtFlags,
        map_flags: MapFlags,
        round_to_min: bool,
        allocate_at_once: bool,
    ) -> Result<VirtPageFrame, SystemError> {
        let (page, notifications) = match self.map_anonymous_collect(
            start_vaddr,
            len,
            prot_flags,
            map_flags,
            round_to_min,
            allocate_at_once,
        ) {
            Ok(outcome) => outcome,
            Err(failure) => {
                debug_assert!(
                    failure.notifications.is_empty(),
                    "locked map_anonymous caller must not replace existing VMAs"
                );
                if !failure.notifications.is_empty() {
                    error!("locked map_anonymous failed after replacing existing VMAs");
                    return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                }
                return Err(failure.err);
            }
        };
        debug_assert!(
            notifications.is_empty(),
            "locked map_anonymous caller must not replace existing VMAs"
        );
        if !notifications.is_empty() {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }
        Ok(page)
    }

    #[allow(clippy::too_many_arguments)]
    fn map_anonymous_collect(
        &mut self,
        start_vaddr: VirtAddr,
        len: usize,
        prot_flags: ProtFlags,
        map_flags: MapFlags,
        round_to_min: bool,
        allocate_at_once: bool,
    ) -> Result<(VirtPageFrame, VmaCloseNotifications), MmapFailure> {
        let allocate_at_once = if MMArch::PAGE_FAULT_ENABLED {
            allocate_at_once
        } else {
            true
        };
        // debug!("map_anonymous: start_vaddr = {:?}", start_vaddr);
        // debug!("map_anonymous: len(no align) = {}", len);

        let len = page_align_up(len);

        // debug!("map_anonymous: len = {}", len);

        let fixed_hint = map_flags.intersects(MapFlags::MAP_FIXED | MapFlags::MAP_FIXED_NOREPLACE);
        let (start_page, notifications) = self.mmap_collect(
            AddressSpace::round_mmap_hint(start_vaddr, round_to_min, fixed_hint),
            PageFrameCount::from_bytes(len).unwrap(),
            prot_flags,
            map_flags,
            move |page, count, vm_flags, flags, mapper, flusher| {
                if allocate_at_once {
                    let vma =
                        VMA::zeroed(page, count, vm_flags, flags, mapper, flusher, None, None)?;
                    // 如果是共享匿名映射，则分配稳定身份
                    if vm_flags.contains(VmFlags::VM_SHARED) {
                        let mut g = vma.lock();
                        g.shared_anon = Some(AnonSharedMapping::new(count.data()));
                        // Set backing_pgoff to 0 as the base offset for shared-anon mappings.
                        g.backing_pgoff = Some(0);
                    }
                    Ok(vma)
                } else {
                    let vma = LockedVMA::new(VMA::new(
                        VirtRegion::new(page.virt_address(), count.data() * MMArch::PAGE_SIZE),
                        vm_flags,
                        flags,
                        None,
                        None,
                        false,
                    ));
                    if vm_flags.contains(VmFlags::VM_SHARED) {
                        let mut g = vma.lock();
                        g.shared_anon = Some(AnonSharedMapping::new(count.data()));
                        g.backing_pgoff = Some(0);
                    }
                    Ok(vma)
                }
            },
        )?;

        self.post_map_population(start_page.virt_address(), len, map_flags);

        return Ok((start_page, notifications));
    }

    /// 向进程的地址空间映射页面
    ///
    /// # 参数
    ///
    /// - `addr`：映射的起始地址，如果为`None`，则由内核自动分配
    /// - `page_count`：映射的页面数量
    /// - `prot_flags`：保护标志
    /// - `map_flags`：映射标志
    /// - `map_func`：映射函数，用于创建VMA
    ///
    /// # Returns
    ///
    /// 返回映射的起始虚拟页帧
    ///
    /// # Errors
    ///
    /// - `EINVAL`：参数错误
    fn mmap_collect<
        F: FnOnce(
            VirtPageFrame,
            PageFrameCount,
            VmFlags,
            EntryFlags<MMArch>,
            &mut PageMapper,
            &mut dyn Flusher<MMArch>,
        ) -> Result<Arc<LockedVMA>, SystemError>,
    >(
        &mut self,
        addr: Option<VirtAddr>,
        page_count: PageFrameCount,
        prot_flags: ProtFlags,
        map_flags: MapFlags,
        map_func: F,
    ) -> Result<(VirtPageFrame, VmaCloseNotifications), MmapFailure> {
        if page_count == PageFrameCount::new(0) {
            return Err(SystemError::EINVAL.into());
        }
        // debug!("mmap: addr: {addr:?}, page_count: {page_count:?}, prot_flags: {prot_flags:?}, map_flags: {map_flags:?}");

        let vm_flags = VmFlags::from(prot_flags)
            | VmFlags::from(map_flags)
            | self.mlock_future
            | VmFlags::VM_MAYREAD
            | VmFlags::VM_MAYWRITE
            | VmFlags::VM_MAYEXEC;

        if vm_flags.contains(VmFlags::VM_LOCKED) {
            let error = if map_flags.contains(MapFlags::MAP_LOCKED) && !Self::has_mlock_quota() {
                SystemError::EPERM
            } else {
                SystemError::EAGAIN_OR_EWOULDBLOCK
            };
            self.check_mlock_rlimit_for_pages(page_count.data(), error)?;
        }

        let mut notifications = VmaCloseNotifications::default();
        macro_rules! mmap_fail {
            ($err:expr) => {
                return Err(MmapFailure {
                    err: $err,
                    notifications,
                })
            };
        }
        macro_rules! mmap_try {
            ($expr:expr) => {
                match $expr {
                    Ok(value) => value,
                    Err(err) => mmap_fail!(err),
                }
            };
        }

        // 先只解析目标区域；MAP_FIXED 的破坏性替换要等前置检查完成后再提交。
        let region = match addr {
            Some(vaddr) => {
                mmap_try!(self.find_free_at_prepare(
                    self.mmap_min,
                    vaddr,
                    page_count.bytes(),
                    map_flags,
                ))
            }
            None => self
                .mappings
                .find_free(self.mmap_min, page_count.bytes())
                .ok_or(SystemError::ENOMEM)?,
        };

        self.check_rlimit_as_for_region(region, page_count.bytes(), map_flags)?;

        if map_flags.contains(MapFlags::MAP_FIXED) && self.mappings.has_conflict(region) {
            let close_notifications = mmap_try!(self.munmap_collect(
                VirtPageFrame::new(region.start()),
                PageFrameCount::from_bytes(region.size()).unwrap(),
            ));
            notifications.extend(close_notifications);
        }

        let page = VirtPageFrame::new(region.start());
        // debug!("mmap: page: {:?}, region={region:?}", page.virt_address());

        let new_locked_vm = if vm_flags.contains(VmFlags::VM_LOCKED) {
            Some(
                self.locked_vm
                    .checked_add(page_count.data())
                    .ok_or(SystemError::ENOMEM)?,
            )
        } else {
            None
        };

        compiler_fence(Ordering::SeqCst);
        // New mapping: the new region had no prior PTE, no TLB invalidation needed.
        // Use DeferredFlusher to silently consume internal PageFlush tokens.
        // If MAP_FIXED support for overwriting existing mappings is added in the future,
        // the caller should first munmap via MmuGather to release old mappings.
        let mut flusher = crate::mm::page::DeferredFlusher::new();
        compiler_fence(Ordering::SeqCst);
        // 映射页面，并将VMA插入到地址空间的VMA列表中
        let new_vma = {
            let Some(mm) = self.outer_addr_space() else {
                mmap_fail!(SystemError::EFAULT);
            };
            let _pt_edit = mm.page_table_edit();
            mmap_try!(map_func(
                page,
                page_count,
                vm_flags,
                EntryFlags::from_prot_flags(prot_flags, true),
                &mut self.user_mapper.utable,
                &mut flusher,
            ))
        };
        let new_present_pages = if new_vma.mapped() {
            page_count.data()
        } else {
            0
        };
        self.mappings.insert_vma(new_vma);
        if let Some(new_locked_vm) = new_locked_vm {
            self.locked_vm = new_locked_vm;
        }
        if let Some(mm) = self.outer_addr_space() {
            mm.account_present_pages_add(new_present_pages);
        }

        return Ok((page, notifications));
    }

    /// 重映射内存区域
    ///
    /// # 参数
    ///
    /// - `old_vaddr`：原映射的起始地址
    /// - `old_len`：原映射的长度
    /// - `new_len`：重新映射的长度
    /// - `mremap_flags`：重映射标志
    /// - `new_vaddr`：重新映射的起始地址
    /// - `vm_flags`：旧内存区域标志
    ///
    /// # Returns
    ///
    /// 返回重映射的起始虚拟页帧地址
    ///
    /// # Errors
    ///
    /// - `EINVAL`：参数错误
    fn mremap(
        &mut self,
        old_vaddr: VirtAddr,
        mut old_len: usize,
        new_len: usize,
        mremap_flags: MremapFlags,
        new_vaddr: VirtAddr,
        vm_flags: VmFlags,
    ) -> Result<MremapOutcome, MremapFailure> {
        let fixed_new_region = if mremap_flags.contains(MremapFlags::MREMAP_FIXED) {
            if !new_vaddr.check_aligned(MMArch::PAGE_SIZE) {
                return Err(SystemError::EINVAL.into());
            }
            let new_region = Self::checked_user_region(new_vaddr, new_len)?;
            let old_end = old_vaddr.data().wrapping_add(old_len);
            let new_end = new_vaddr
                .data()
                .checked_add(new_len)
                .ok_or(SystemError::EINVAL)?;
            if old_end > new_vaddr.data() && new_end > old_vaddr.data() {
                return Err(SystemError::EINVAL.into());
            }
            if old_len != 0 {
                let old_region = Self::checked_user_region(old_vaddr, old_len)?;
                debug_assert!(!old_region.collide(&new_region));
            }
            Some(new_region)
        } else {
            None
        };
        // 初始化内存区域保护标志
        let prot_flags: ProtFlags = vm_flags.into();
        let mut notifications = VmaCloseNotifications::default();
        macro_rules! mremap_fail {
            ($err:expr) => {
                return Err(MremapFailure {
                    err: $err,
                    notifications,
                })
            };
        }
        macro_rules! mremap_try {
            ($expr:expr) => {
                match $expr {
                    Ok(value) => value,
                    Err(err) => mremap_fail!(err),
                }
            };
        }

        if mremap_flags.contains(MremapFlags::MREMAP_FIXED)
            && self.mappings.contains(old_vaddr).is_none()
        {
            mremap_fail!(SystemError::EFAULT);
        }

        if mremap_flags.contains(MremapFlags::MREMAP_FIXED) {
            let start_page = VirtPageFrame::new(new_vaddr);
            let page_count = PageFrameCount::from_bytes(new_len).unwrap();
            notifications.extend(mremap_try!(self.munmap_collect(start_page, page_count)));
        }
        if mremap_flags.contains(MremapFlags::MREMAP_FIXED) && old_len > new_len {
            notifications.extend(mremap_try!(self.munmap_collect(
                VirtPageFrame::new(old_vaddr + new_len),
                PageFrameCount::from_bytes(old_len - new_len).unwrap(),
            )));
            old_len = new_len;
        }
        // 读取旧 VMA 的后备信息（file/shared-anon）以及页偏移基址。
        // MREMAP_FIXED 在上方可能已拆掉目标区间以及 shrink tail；重新查询源
        // VMA，避免使用可能被 split 后失效的旧缓存。
        let Some(old_vma) = self.mappings.contains(old_vaddr) else {
            mremap_fail!(SystemError::EFAULT);
        };
        let (old_region, vm_file, shared_anon, base_pgoff, sysv_shm) = {
            let g = old_vma.lock();
            let region = *g.region();
            let vma_start = region.start();
            let off_pages =
                (old_vaddr.data().saturating_sub(vma_start.data())) >> MMArch::PAGE_SHIFT;
            let base = g
                .backing_page_offset()
                .unwrap_or(0)
                .saturating_add(off_pages);
            (
                region,
                g.vm_file(),
                g.shared_anon.clone(),
                base,
                g.sysv_shm(),
            )
        };

        // 构造目标映射 flags：mremap 需要保留 shared/private 语义，并区分 anon/file。
        let mut map_flags: MapFlags = vm_flags.into();
        if map_flags.contains(MapFlags::MAP_SHARED) {
            // ok
        } else {
            map_flags |= MapFlags::MAP_PRIVATE;
        }
        if vm_file.is_none() {
            map_flags |= MapFlags::MAP_ANONYMOUS;
        }
        if mremap_flags.contains(MremapFlags::MREMAP_FIXED) {
            map_flags |= MapFlags::MAP_FIXED;
        }

        let dontunmap_flag = mremap_flags.contains(MremapFlags::MREMAP_DONTUNMAP);
        let locked_source = vm_flags.contains(VmFlags::VM_LOCKED);
        let sysv_mremap = sysv_shm.is_some();
        let source_len = old_len;
        let Some(max_old_len) = old_region.end().data().checked_sub(old_vaddr.data()) else {
            mremap_fail!(SystemError::EINVAL);
        };
        if source_len > max_old_len {
            mremap_fail!(SystemError::EFAULT);
        }
        let source_region = VirtRegion::new(old_vaddr, source_len);
        if dontunmap_flag {
            if vm_flags.intersects(VmFlags::VM_DONTEXPAND | VmFlags::VM_PFNMAP) {
                mremap_fail!(SystemError::EINVAL);
            }
            let Some(old_end) = old_vaddr.data().checked_add(old_len) else {
                mremap_fail!(SystemError::EINVAL);
            };
            let Some(new_end) = new_vaddr.data().checked_add(new_len) else {
                mremap_fail!(SystemError::EINVAL);
            };
            if old_end > new_vaddr.data() && new_end > old_vaddr.data() {
                mremap_fail!(SystemError::EINVAL);
            }
        }
        if locked_source {
            let additional_locked_pages = if old_len == 0 {
                new_len >> MMArch::PAGE_SHIFT
            } else if dontunmap_flag {
                0
            } else if new_len > old_len {
                (new_len - old_len) >> MMArch::PAGE_SHIFT
            } else {
                0
            };
            if additional_locked_pages != 0 {
                mremap_try!(self.check_mlock_rlimit_for_pages(
                    additional_locked_pages,
                    SystemError::EAGAIN_OR_EWOULDBLOCK,
                ));
            }
        }
        let as_delta = if old_len == 0 || dontunmap_flag {
            new_len
        } else {
            new_len.saturating_sub(old_len)
        };
        if as_delta != 0 {
            mremap_try!(self.check_rlimit_as_for_bytes(as_delta));
        }

        // 是否允许移动（Linux: 只有 MAYMOVE / FIXED 才能移动）
        let can_move = mremap_flags.contains(MremapFlags::MREMAP_MAYMOVE)
            || mremap_flags.contains(MremapFlags::MREMAP_FIXED);

        // Linux: old_len==0 表示“复制/重复映射”共享区域（DOS-emu legacy）。
        // - 仅允许对共享映射进行
        // - 没有 MAYMOVE/FIXED 时返回 ENOMEM
        if old_len == 0 {
            if !vm_flags.intersects(VmFlags::VM_SHARED | VmFlags::VM_MAYSHARE) {
                mremap_fail!(SystemError::EINVAL);
            }
            if !can_move {
                mremap_fail!(SystemError::ENOMEM);
            }
        }

        if mremap_flags.contains(MremapFlags::MREMAP_FIXED) {
            if let Err(err) = check_mmap_min_addr(new_vaddr, self.mmap_min) {
                mremap_fail!(err);
            }
        }

        // 不允许移动时，只能尝试原地扩展。
        if !can_move {
            if new_len <= old_len {
                return Ok(MremapOutcome {
                    addr: old_vaddr,
                    notifications: VmaCloseNotifications::default(),
                });
            }

            // Linux only allows in-place expansion when the old range reaches the VMA end.
            if old_len != max_old_len {
                mremap_fail!(SystemError::ENOMEM);
            }

            let grow = new_len - old_len;
            let Some(grown_region_size) = old_region.size().checked_add(grow) else {
                mremap_fail!(SystemError::ENOMEM);
            };
            let Some(grown_end) = old_region.start().data().checked_add(grown_region_size) else {
                mremap_fail!(SystemError::ENOMEM);
            };
            if grown_end > MMArch::USER_END_VADDR.data() {
                mremap_fail!(SystemError::ENOMEM);
            }
            let locked_vm_after_grow = if locked_source {
                let Some(locked_vm_after_grow) =
                    self.locked_vm.checked_add(grow >> MMArch::PAGE_SHIFT)
                else {
                    mremap_fail!(SystemError::ENOMEM);
                };
                Some(locked_vm_after_grow)
            } else {
                None
            };
            let grow_region = VirtRegion::new(old_vaddr + old_len, grow);
            if self.mappings.has_conflict(grow_region) {
                mremap_fail!(SystemError::ENOMEM);
            }

            let Some(removed) = self.mappings.remove_vma(&old_region) else {
                mremap_fail!(SystemError::EINVAL);
            };
            removed.lock().set_region_size(grown_region_size);
            self.mappings.insert_vma(removed);
            if let Some(locked_vm_after_grow) = locked_vm_after_grow {
                self.locked_vm = locked_vm_after_grow;
                self.best_effort_locked_population(old_vaddr + old_len, grow, vm_flags);
            }
            return Ok(MremapOutcome {
                addr: old_vaddr,
                notifications: VmaCloseNotifications::default(),
            });
        }

        // 需要创建一个新映射并迁移（FIXED 或 MAYMOVE）。
        // 注意：必须避免在持有地址空间写锁时触碰用户地址（会触发缺页递归死锁）。
        // Linux 的 mremap 通过移动/复制页表项实现，而不是字节拷贝。

        let new_region: VirtRegion = if let Some(new_region) = fixed_new_region {
            new_region
        } else if dontunmap_flag {
            let (region, close_notifications) = mremap_try!(self.find_free_at_collect(
                self.mmap_min,
                new_vaddr,
                new_len,
                map_flags,
            ));
            notifications.extend(close_notifications);
            region
        } else {
            let Some(new_region) = self.mappings.find_free(self.mmap_min, new_len) else {
                mremap_fail!(SystemError::ENOMEM);
            };
            new_region
        };

        let entry_flags = EntryFlags::from_prot_flags(prot_flags, true);
        let Some(mm) = self.outer_addr_space() else {
            mremap_fail!(SystemError::EFAULT);
        };
        let remove_source_vma_on_commit =
            !dontunmap_flag && old_len != 0 && new_region.start() != old_vaddr;
        let split_source_on_commit = old_len != 0 && source_region != old_region && !dontunmap_flag;

        let locked_vm_after_move_commit = if locked_source {
            let new_pages = new_len >> MMArch::PAGE_SHIFT;
            let old_pages = source_len >> MMArch::PAGE_SHIFT;
            if old_len == 0 {
                let Some(locked_vm_after_commit) = self.locked_vm.checked_add(new_pages) else {
                    mremap_fail!(SystemError::ENOMEM);
                };
                Some(locked_vm_after_commit)
            } else if dontunmap_flag {
                // Linux move_vma() clears VM_LOCKED on the old VMA for
                // MREMAP_DONTUNMAP but deliberately leaves mm->locked_vm
                // unchanged because the source range is not unmapped.
                Some(self.locked_vm)
            } else {
                let Some(locked_after_add) = self.locked_vm.checked_add(new_pages) else {
                    mremap_fail!(SystemError::ENOMEM);
                };
                let Some(locked_vm_after_commit) = locked_after_add.checked_sub(old_pages) else {
                    mremap_fail!(SystemError::ENOMEM);
                };
                Some(locked_vm_after_commit)
            }
        } else {
            None
        };
        let mut source_split_lifecycle = if split_source_on_commit {
            Some(mremap_try!(old_vma.prepare_split_lifecycle(source_region)))
        } else {
            None
        };
        if let Some(sysv_shm) = sysv_shm.as_ref() {
            if let Err(err) = sysv_shm.open_vma() {
                if let Some(lifecycle) = source_split_lifecycle.take() {
                    lifecycle.rollback_into(&mut notifications);
                }
                mremap_fail!(err);
            }
        }

        // 创建目标 VMA（初始不映射物理页；存在的页表项会在下面被移动/复制）。
        let new_vma: Arc<LockedVMA> = {
            let vma = LockedVMA::new(VMA::new(
                new_region,
                vm_flags,
                entry_flags,
                vm_file.clone(),
                if vm_file.is_some() || shared_anon.is_some() {
                    Some(base_pgoff)
                } else {
                    None
                },
                false,
            ));
            if let Some(shared) = shared_anon.clone() {
                let mut vg = vma.lock();
                vg.shared_anon = Some(shared);
                vg.backing_pgoff = Some(base_pgoff);
            }
            if let Some(sysv_shm) = sysv_shm.clone() {
                vma.lock().set_sysv_shm(Some(sysv_shm));
            }
            vma
        };

        // Linux mremap moves/duplicates an existing VMA; it does not call the
        // filesystem mmap hook again. The file mapping was already accepted
        // when the source VMA was created.
        self.mappings.insert_vma(new_vma.clone());
        let move_len = core::cmp::min(source_len, new_len);

        // mremap does not free physical pages; old PTEs are migrated to the new VMA, while
        // old_len==0 keeps the legacy duplicate-mapping behavior.
        // using MmuGather here is solely for a unified cross-core TLB shootdown at the end.
        let mut tlb = MmuGather::gather(&mm);

        // 迁移/复制已存在的页表映射。
        // 阶段 A：先完整安装目标 PTE，不破坏源 PTE；失败时只需删除目标 PTE。
        // 阶段 B：目标 PTE 全部安装成功后，再不可失败地移除源 PTE 并切换 vma_set。
        // Linux 的 MREMAP_DONTUNMAP 保留旧 VMA，但页表仍会迁移；不能长期保留源 PTE。
        let mapper = &mut self.user_mapper.utable;
        let old_vma = old_vma.clone();
        let mut installed_target_pte = false;
        let mut installed_target_present_pages = 0usize;
        let mut removed_source_present_pages = 0usize;

        {
            let _pt_edit = mm.page_table_edit();
            let mut page_manager_guard = page_manager_lock();
            let mut migrated = Vec::new();
            let mut err = None;
            let mut off = 0usize;
            while off < move_len {
                let src = old_vaddr + off;
                let dst = new_region.start() + off;
                if let Some((paddr, src_flags)) = mapper.translate(src) {
                    let Some(flush) = (unsafe { mapper.map_phys(dst, paddr, src_flags) }) else {
                        err = Some(SystemError::ENOMEM);
                        break;
                    };
                    unsafe { flush.ignore() };
                    tlb.accumulate_range(dst);
                    installed_target_pte = true;
                    installed_target_present_pages += 1;
                    page_manager_guard
                        .get_unwrap(&paddr)
                        .write()
                        .insert_vma(new_vma.clone(), locked_source);

                    migrated.push((src, dst, paddr, src_flags));
                }
                off += MMArch::PAGE_SIZE;
            }

            if let Some(err) = err {
                for (_src, dst, paddr, _src_flags) in migrated.into_iter().rev() {
                    if let Some((_unmapped_paddr, _flags, flush)) =
                        unsafe { mapper.unmap_phys_preserve_tables(dst) }
                    {
                        unsafe { flush.ignore() };
                        tlb.accumulate_range(dst);
                    }
                    if let Some(page) = page_manager_guard.get(&paddr) {
                        page.write().remove_vma(new_vma.as_ref());
                    }
                }

                self.mappings.remove_vma(&new_region);
                drop(page_manager_guard);
                tlb.finish();
                if let Some(sysv_shm) = sysv_shm.as_ref() {
                    notifications.sysv.push(sysv_shm.clone());
                }
                if let Some(lifecycle) = source_split_lifecycle.take() {
                    lifecycle.rollback_into(&mut notifications);
                }
                mremap_fail!(err);
            }

            if old_len != 0 {
                for (src, _dst, paddr, _src_flags) in migrated {
                    if let Some((_paddr2, _flags2, flush)) =
                        unsafe { mapper.unmap_phys_preserve_tables(src) }
                    {
                        unsafe { flush.ignore() };
                        tlb.accumulate_range(src);
                        removed_source_present_pages += 1;
                    } else {
                        panic!("mremap commit lost expected source PTE at {:?}", src);
                    }

                    let page = page_manager_guard.get_unwrap(&paddr);
                    let mut pg = page.write();
                    pg.remove_vma(old_vma.as_ref());
                }
            }
        }
        if installed_target_pte {
            new_vma.lock().set_mapped(true);
        }
        if installed_target_present_pages >= removed_source_present_pages {
            mm.account_present_pages_add(
                installed_target_present_pages - removed_source_present_pages,
            );
        } else {
            mm.account_present_pages_sub(
                removed_source_present_pages - installed_target_present_pages,
            );
        }

        if sysv_mremap || remove_source_vma_on_commit || (locked_source && dontunmap_flag) {
            let mut source_vma = old_vma.clone();
            let mut split_before = None;
            let mut split_after = None;

            if split_source_on_commit {
                let removed = self
                    .mappings
                    .remove_vma(&old_region)
                    .expect("validated mremap source VMA must exist");
                debug_assert!(Arc::ptr_eq(&removed, &old_vma));
                let split_result = removed
                    .extract(source_region, &self.user_mapper.utable)
                    .expect("validated mremap source region must split");
                source_vma = split_result.middle;
                split_before = split_result.prev;
                split_after = split_result.after;
            }

            if locked_source && dontunmap_flag {
                self.update_present_page_mlock_refs(
                    &source_vma,
                    old_region.start(),
                    old_region.end(),
                    true,
                    false,
                );
                let clear_locked = |vma: &Arc<LockedVMA>| {
                    let mut guard = vma.lock();
                    let unlocked_flags = *guard.vm_flags() & VmFlags::VM_LOCKED_CLEAR_MASK;
                    guard.set_vm_flags(unlocked_flags);
                    guard.set_flags();
                };
                clear_locked(&source_vma);
            }

            if let Some(before) = split_before {
                self.mappings.insert_vma(before);
            }
            if let Some(after) = split_after {
                self.mappings.insert_vma(after);
            }
            if let Some(lifecycle) = source_split_lifecycle.take() {
                lifecycle.commit();
            }
            if remove_source_vma_on_commit {
                if split_source_on_commit {
                    source_vma.unmap(&mut self.user_mapper.utable, &mut tlb);
                    source_vma.lock().set_mapped(false);
                    if let Some(notification) = Self::collect_vma_close(&source_vma, source_region)
                    {
                        notifications.vma.push(notification);
                    }
                    if let Some(notification) = Self::collect_sysv_shm_close(&source_vma) {
                        notifications.sysv.push(notification);
                    }
                } else {
                    let removed = self
                        .mappings
                        .remove_vma(&old_region)
                        .expect("validated mremap source VMA must exist");
                    removed.unmap(&mut self.user_mapper.utable, &mut tlb);
                    removed.lock().set_mapped(false);
                    if let Some(notification) = Self::collect_vma_close(&removed, old_region) {
                        notifications.vma.push(notification);
                    }
                    if let Some(notification) = Self::collect_sysv_shm_close(&removed) {
                        notifications.sysv.push(notification);
                    }
                }
            }
            if split_source_on_commit && !remove_source_vma_on_commit {
                self.mappings.insert_vma(source_vma);
            }

            if let Some(locked_vm_after_commit) = locked_vm_after_move_commit {
                self.locked_vm = locked_vm_after_commit;
            }
            tlb.finish();

            if locked_source && new_len > old_len {
                self.best_effort_locked_population(
                    new_region.start() + old_len,
                    new_len - old_len,
                    vm_flags,
                );
            }

            return Ok(MremapOutcome {
                addr: new_region.start(),
                notifications,
            });
        }

        if let Some(locked_vm_after_commit) = locked_vm_after_move_commit {
            self.locked_vm = locked_vm_after_commit;
        }
        tlb.finish();

        if locked_source && new_len > old_len {
            self.best_effort_locked_population(
                new_region.start() + old_len,
                new_len - old_len,
                vm_flags,
            );
        }

        Ok(MremapOutcome {
            addr: new_region.start(),
            notifications,
        })
    }

    /// 取消进程的地址空间中的映射
    ///
    /// # 参数
    ///
    /// - `start_page`：起始页帧
    /// - `page_count`：取消映射的页帧数量
    ///
    /// # Errors
    ///
    /// - `EINVAL`：参数错误
    /// - `ENOMEM`：内存不足
    /// - `EFAULT`：VMA 状态异常
    pub fn munmap(
        &mut self,
        start_page: VirtPageFrame,
        page_count: PageFrameCount,
    ) -> Result<(), SystemError> {
        let notifications = self.munmap_collect(start_page, page_count)?;
        Self::notify_close_notifications(notifications);
        Ok(())
    }

    fn munmap_collect(
        &mut self,
        start_page: VirtPageFrame,
        page_count: PageFrameCount,
    ) -> Result<VmaCloseNotifications, SystemError> {
        defer!({
            compiler_fence(Ordering::SeqCst);
        });

        // 获取取消映射操作关联的 VMAS （用户传入的区域可能横跨多个 VMA）
        let region_to_unmap = VirtRegion::new(start_page.virt_address(), page_count.bytes());
        let vmas_related: Vec<Arc<LockedVMA>> = self.mappings.conflicts(region_to_unmap);

        // Use MmuGather: clear PTEs + stash pages first, then unified shootdown, and finally free physical pages (INV-3)
        let mm = self.outer_addr_space().ok_or(SystemError::EFAULT)?;
        let mut tlb = MmuGather::gather(&mm);
        let mut notifications = VmaCloseNotifications::default();
        let mut plans: Vec<MunmapVmaPlan> = Vec::with_capacity(vmas_related.len());
        let mut unmapped_vmas: Vec<Arc<LockedVMA>> = Vec::with_capacity(vmas_related.len());
        let mut locked_vm_after_commit = self.locked_vm;

        // 遍历每个相关的 VMA，将当前的 VMA 拆分为可能的三块 VMA，然后删除与需要删除的区域相交的部分。
        // 示意图：对每个与 region_to_unmap 相交的 VMA，按交集拆分成三段（before / intersection / after），
        // 然后仅对 intersection 段执行解除映射；before/after 重新插回 mappings。
        //
        //          cur_vma.region (原 VMA)
        //      [------------------------------]
        //                region_to_unmap
        //            [----------]
        //                 ||
        //                 \/
        //      before         intersection          after
        //   [--------]      [----------]         [--------]
        //      keep            unmap                keep
        //
        // 注意：用户传入的 region_to_unmap 可能跨多个 VMA，因此需要对每个相关 VMA 分别处理。
        //
        // 第一阶段只做校验和 SysV split side 预打开，不修改 mappings。这样后面的
        // VMA 若因 RMID/引用限制导致 open_vma 失败，前面 VMA 不会已经被删除。
        for cur_vma in vmas_related {
            let (original_region, intersection, locked) = {
                let guard = cur_vma.lock();
                let original_region = *guard.region();
                let intersection = original_region
                    .intersect(&region_to_unmap)
                    .ok_or(SystemError::EFAULT)?;
                (
                    original_region,
                    intersection,
                    guard.vm_flags().contains(VmFlags::VM_LOCKED),
                )
            };
            let locked_vm_after_unmap = if locked {
                locked_vm_after_commit = locked_vm_after_commit
                    .checked_sub(intersection.size() >> MMArch::PAGE_SHIFT)
                    .ok_or(SystemError::EFAULT)?;
                Some(locked_vm_after_commit)
            } else {
                None
            };

            let split_lifecycle = cur_vma.prepare_split_lifecycle(intersection)?;

            plans.push(MunmapVmaPlan {
                original_region,
                intersection,
                locked_vm_after_unmap,
                split_lifecycle,
            });
        }

        plans.reverse();
        while let Some(plan) = plans.pop() {
            let cur_vma = match self.mappings.remove_vma(&plan.original_region) {
                Some(vma) => vma,
                None => return Err(SystemError::EFAULT),
            };
            let (before, after) = {
                let _pt_edit = mm.page_table_edit();
                let Some(split_result) =
                    cur_vma.extract(plan.intersection, &self.user_mapper.utable)
                else {
                    self.mappings.insert_vma(cur_vma.clone());
                    return Err(SystemError::EFAULT);
                };
                let before = split_result.prev;
                let after = split_result.after;
                if let Some(locked_vm_after_unmap) = plan.locked_vm_after_unmap {
                    self.locked_vm = locked_vm_after_unmap;
                }
                cur_vma.unmap(&mut self.user_mapper.utable, &mut tlb);
                (before, after)
            };

            if let Some(notification) = Self::collect_vma_close(&cur_vma, plan.intersection) {
                notifications.vma.push(notification);
            }
            if let Some(notification) = Self::collect_sysv_shm_close(&cur_vma) {
                notifications.sysv.push(notification);
            }

            if let Some(before) = before {
                self.mappings.insert_vma(before);
            }

            if let Some(after) = after {
                self.mappings.insert_vma(after);
            }
            plan.split_lifecycle.commit();
            // Keep the removed VMA alive until after TLB shootdown.  Its drop may
            // destroy the last shared-anon backing object, which can release
            // physical pages that were just unmapped above.
            unmapped_vmas.push(cur_vma);
        }

        // Shootdown first, then free physical pages
        tlb.finish();
        drop(unmapped_vmas);

        Ok(notifications)
    }

    fn detach_sysv_shm(&mut self, addr: VirtAddr) -> Result<VmaCloseNotifications, SystemError> {
        if !addr.check_aligned(MMArch::PAGE_SIZE) {
            return Err(SystemError::EINVAL);
        }

        let mut attach_file = None;
        let mut attach_id = None;
        let mut segment_size = 0usize;
        let mut targets: Vec<Arc<LockedVMA>> = Vec::new();

        for vma in self.mappings.iter_vmas_starting_at(addr) {
            let (region, pgoff, sysv, file) = {
                let guard = vma.lock();
                (
                    *guard.region(),
                    guard.backing_page_offset(),
                    guard.sysv_shm(),
                    guard.vm_file(),
                )
            };
            if !targets.is_empty()
                && region.start().data().saturating_sub(addr.data()) >= segment_size
            {
                break;
            }
            let Some(sysv) = sysv else {
                continue;
            };
            if region.start() < addr {
                continue;
            }
            let Some(pgoff) = pgoff else {
                continue;
            };

            if file.is_none() {
                continue;
            };
            let expected_pgoff = (region.start().data() - addr.data()) >> MMArch::PAGE_SHIFT;
            let first_match = attach_file.is_none();
            if first_match {
                if pgoff != expected_pgoff {
                    continue;
                }
                segment_size = page_align_up(sysv.size());
                attach_id = Some(sysv.attach_id());
                attach_file = file;
            } else {
                if region.end().data().saturating_sub(addr.data()) > segment_size {
                    break;
                }
                if attach_id != Some(sysv.attach_id()) {
                    continue;
                }
                if pgoff != expected_pgoff {
                    continue;
                }
                let Some(expected_file) = attach_file.as_ref() else {
                    continue;
                };
                let Some(file) = file else {
                    continue;
                };
                if !Arc::ptr_eq(&file, expected_file) {
                    continue;
                }
            }
            targets.push(vma.clone());
        }

        if targets.is_empty() {
            return Err(SystemError::EINVAL);
        }

        let locked_pages = targets.iter().try_fold(0usize, |acc, target| {
            let guard = target.lock();
            if guard.vm_flags().contains(VmFlags::VM_LOCKED) {
                acc.checked_add(guard.region().size() >> MMArch::PAGE_SHIFT)
                    .ok_or(SystemError::EOVERFLOW)
            } else {
                Ok(acc)
            }
        })?;
        let new_locked_vm = self.locked_vm.checked_sub(locked_pages).ok_or_else(|| {
            error!(
                "shmdt locked_vm accounting underflow: locked_vm={}, pages={}",
                self.locked_vm, locked_pages
            );
            debug_assert!(
                false,
                "shmdt locked_vm accounting underflow: locked_vm={}, pages={}",
                self.locked_vm, locked_pages
            );
            SystemError::EFAULT
        })?;

        let mm = self.outer_addr_space().ok_or(SystemError::EFAULT)?;
        let _pt_edit = mm.page_table_edit();
        let mut tlb = MmuGather::gather(&mm);
        let mut notifications = VmaCloseNotifications::default();

        for target in targets {
            let region = *target.lock().region();
            let vma = self
                .mappings
                .remove_vma(&region)
                .ok_or(SystemError::EFAULT)?;
            if let Some(notification) = Self::collect_vma_close(&vma, region) {
                notifications.vma.push(notification);
            }
            if let Some(notification) = Self::collect_sysv_shm_close(&vma) {
                notifications.sysv.push(notification);
            }
            vma.unmap(&mut self.user_mapper.utable, &mut tlb);
        }
        self.locked_vm = new_locked_vm;
        tlb.finish();

        Ok(notifications)
    }

    fn collect_sysv_shm_close(vma: &Arc<LockedVMA>) -> Option<Arc<SysVShmAttach>> {
        vma.lock().sysv_shm()
    }

    fn notify_sysv_shm_close(notification: Arc<SysVShmAttach>) {
        notification.close_vma();
    }

    fn notify_close_notifications(notifications: VmaCloseNotifications) {
        for notification in notifications.vma {
            Self::notify_vma_close(notification);
        }
        for notification in notifications.sysv {
            Self::notify_sysv_shm_close(notification);
        }
    }

    fn collect_vma_close(vma: &Arc<LockedVMA>, region: VirtRegion) -> Option<VmaCloseNotification> {
        let (file, vm_flags) = {
            let guard = vma.lock();
            let file = guard.vm_file()?;
            (file, *guard.vm_flags())
        };

        if vm_flags.contains(VmFlags::VM_SHARED | VmFlags::VM_WRITE) {
            Some(VmaCloseNotification {
                file,
                region,
                vm_flags,
            })
        } else {
            None
        }
    }

    fn notify_vma_close(notification: VmaCloseNotification) {
        notification.file.inode().fs().vma_close(
            &notification.file,
            notification.region,
            notification.vm_flags,
        );
    }

    pub fn mprotect(
        &mut self,
        start_page: VirtPageFrame,
        page_count: PageFrameCount,
        prot_flags: ProtFlags,
    ) -> Result<(), SystemError> {
        // debug!(
        //     "mprotect: start_page: {:?}, page_count: {:?}, prot_flags:{prot_flags:?}",
        //     start_page,
        //     page_count
        // );
        let mm = self.outer_addr_space().ok_or(SystemError::EFAULT)?;
        let mut tlb = MmuGather::gather(&mm);

        let mapper = &mut self.user_mapper.utable;
        let region = VirtRegion::new(start_page.virt_address(), page_count.bytes());
        // debug!("mprotect: region: {:?}", region);

        let (regions, has_unmapped) = self.mappings.conflicts_with_unmapped(region);
        if has_unmapped {
            return Err(SystemError::ENOMEM);
        }
        // debug!("mprotect: regions: {:?}", regions);

        let mut plans = Vec::with_capacity(regions.len());
        for r in &regions {
            // debug!("mprotect: r: {:?}", r);
            let (original_region, new_vm_flags) = {
                let guard = r.lock();
                if !guard.can_have_flags(prot_flags) {
                    return Err(SystemError::EACCES);
                }
                let old_vm_flags = *guard.vm_flags();
                let access_flags = VmFlags::VM_READ | VmFlags::VM_WRITE | VmFlags::VM_EXEC;
                let new_vm_flags = (old_vm_flags & !access_flags) | VmFlags::from(prot_flags);
                if new_vm_flags == old_vm_flags {
                    continue;
                }
                if let Some(file) = guard.vm_file() {
                    file.inode().fs().mprotect(old_vm_flags, new_vm_flags)?;
                }
                (*guard.region(), new_vm_flags)
            };
            let intersection = original_region.intersect(&region).unwrap();
            let split_lifecycle = r.prepare_split_lifecycle(intersection)?;
            plans.push(MprotectVmaPlan {
                original_region,
                intersection,
                new_vm_flags,
                split_lifecycle,
            });
        }

        for plan in plans {
            let r = match self.mappings.remove_vma(&plan.original_region) {
                Some(vma) => vma,
                None => return Err(SystemError::EFAULT),
            };

            let remap_result: Result<VmaSplitSides, SystemError> = {
                let _pt_edit = mm.page_table_edit();
                let split_result = r
                    .extract(plan.intersection, mapper)
                    .expect("Failed to extract VMA");

                let mut r_guard = r.lock();
                r_guard.set_vm_flags(plan.new_vm_flags);

                let new_flags: EntryFlags<MMArch> = MMArch::vm_get_page_prot(plan.new_vm_flags);

                r_guard.remap(new_flags, mapper, &mut tlb);
                Ok((split_result.prev, split_result.after))
            };
            let (before, after) = match remap_result {
                Ok(result) => result,
                Err(err) => {
                    self.mappings.insert_vma(r);
                    return Err(err);
                }
            };

            if let Some(before) = before {
                self.mappings.insert_vma(before);
            }
            if let Some(after) = after {
                self.mappings.insert_vma(after);
            }
            self.mappings.insert_vma(r);
            plan.split_lifecycle.commit();
        }

        // Unified shootdown. mprotect does not free physical pages; tlb.finish() mainly flushes the TLB.
        tlb.finish();
        return Ok(());
    }

    pub fn mincore(
        &self,
        start_page: VirtPageFrame,
        page_count: PageFrameCount,
        vec: &mut [u8],
    ) -> Result<(), SystemError> {
        let mapper = &self.user_mapper.utable;

        if self.mappings.contains(start_page.virt_address()).is_none() {
            return Err(SystemError::ENOMEM);
        }

        let mut last_vaddr = start_page.virt_address();
        let region = VirtRegion::new(start_page.virt_address(), page_count.bytes());
        let mut vmas = self.mappings.conflicts(region);
        // 为保证与地址连续性的判断正确，这里按起始地址升序遍历
        vmas.sort_by_key(|v| v.lock().region().start().data());
        let mut offset = 0;
        for v in vmas {
            let region = *v.lock().region();
            // 保证相邻的两个vma连续
            if region.start() != last_vaddr && last_vaddr != start_page.virt_address() {
                return Err(SystemError::ENOMEM);
            }
            let start_vaddr = last_vaddr;
            let end_vaddr = core::cmp::min(region.end(), start_vaddr + page_count.bytes());
            v.do_mincore(mapper, vec, start_vaddr, end_vaddr, offset)?;
            let page_count_this_vma = (end_vaddr - start_vaddr) >> MMArch::PAGE_SHIFT;
            offset += page_count_this_vma;
            last_vaddr = end_vaddr;
        }

        // 校验覆盖完整性：若末尾未覆盖到请求范围，则返回 ENOMEM
        if last_vaddr != region.end() {
            return Err(SystemError::ENOMEM);
        }

        return Ok(());
    }

    pub fn count_unlocked_pages_for_mlock(
        &self,
        start: VirtAddr,
        len: usize,
    ) -> Result<usize, SystemError> {
        let target = Self::checked_user_region(start, len)?;
        let mut vmas = self.mappings.conflicts(target);
        vmas.sort_by_key(|vma| vma.lock().region().start().data());

        let mut cursor = target.start();
        let mut unlocked_pages = 0usize;
        for vma in vmas {
            let guard = vma.lock();
            let region = *guard.region();
            let Some(intersection) = region.intersect(&target) else {
                continue;
            };
            if intersection.start() > cursor {
                return Err(SystemError::ENOMEM);
            }
            if intersection.end() > cursor {
                if !guard.vm_flags().contains(VmFlags::VM_LOCKED) {
                    unlocked_pages = unlocked_pages
                        .checked_add(intersection.size() >> MMArch::PAGE_SHIFT)
                        .ok_or(SystemError::ENOMEM)?;
                }
                cursor = intersection.end();
            }
            if cursor >= target.end() {
                break;
            }
        }

        if cursor != target.end() {
            return Err(SystemError::ENOMEM);
        }
        Ok(unlocked_pages)
    }

    pub fn count_unlocked_pages_for_mlockall(&self) -> Result<usize, SystemError> {
        let mut unlocked_pages = 0usize;
        for vma in self.mappings.iter_vmas() {
            let guard = vma.lock();
            if !guard.vm_flags().contains(VmFlags::VM_LOCKED) {
                unlocked_pages = unlocked_pages
                    .checked_add(guard.region().size() >> MMArch::PAGE_SHIFT)
                    .ok_or(SystemError::ENOMEM)?;
            }
        }

        Ok(unlocked_pages)
    }

    pub fn apply_mlockall_current(&mut self, new_flags: VmFlags) -> Result<(), SystemError> {
        let ranges = self
            .mappings
            .iter_vmas()
            .map(|vma| {
                let guard = vma.lock();
                (guard.region().start(), guard.region().size())
            })
            .collect::<Vec<_>>();

        for (start, len) in ranges {
            self.apply_vma_lock_flags(start, len, new_flags, true)?;
        }

        Ok(())
    }

    pub fn set_mlock_future(&mut self, flags: VmFlags) {
        self.mlock_future = flags;
    }

    pub fn clear_all_vma_lock_flags(&mut self) -> Result<(), SystemError> {
        self.mlock_future = VmFlags::VM_NONE;
        let ranges = self
            .mappings
            .iter_vmas()
            .filter_map(|vma| {
                let guard = vma.lock();
                if guard
                    .vm_flags()
                    .intersects(VmFlags::VM_LOCKED | VmFlags::VM_LOCKONFAULT)
                {
                    Some((guard.region().start(), guard.region().size()))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        for (start, len) in ranges {
            self.apply_vma_lock_flags(start, len, VmFlags::VM_NONE, false)?;
        }

        Ok(())
    }

    pub fn apply_vma_lock_flags(
        &mut self,
        start: VirtAddr,
        len: usize,
        new_flags: VmFlags,
        ignore_populate_errors: bool,
    ) -> Result<(), SystemError> {
        let target = Self::checked_user_region(start, len)?;
        self.count_unlocked_pages_for_mlock(start, len)?;

        let wants_locked = new_flags.contains(VmFlags::VM_LOCKED);
        let mut vmas = self.mappings.conflicts(target);
        vmas.sort_by_key(|vma| vma.lock().region().start().data());

        for cur_vma in vmas {
            let (original_region, intersection, old_flags) = {
                let guard = cur_vma.lock();
                (
                    *guard.region(),
                    guard
                        .region()
                        .intersect(&target)
                        .ok_or(SystemError::EFAULT)?,
                    *guard.vm_flags(),
                )
            };
            let old_locked = old_flags.contains(VmFlags::VM_LOCKED);
            let committed_flags = (old_flags & VmFlags::VM_LOCKED_CLEAR_MASK) | new_flags;
            if committed_flags == old_flags {
                continue;
            }
            let pages = intersection.size() >> MMArch::PAGE_SHIFT;
            let locked_vm_after = if wants_locked && !old_locked {
                Some(
                    self.locked_vm
                        .checked_add(pages)
                        .ok_or(SystemError::ENOMEM)?,
                )
            } else if !wants_locked && old_locked {
                Some(
                    self.locked_vm
                        .checked_sub(pages)
                        .ok_or(SystemError::ENOMEM)?,
                )
            } else {
                None
            };
            let split_lifecycle = cur_vma.prepare_split_lifecycle(intersection)?;
            let cur_vma = match self.mappings.remove_vma(&original_region) {
                Some(vma) => vma,
                None => return Err(SystemError::EFAULT),
            };

            let split_result = cur_vma
                .extract(intersection, &self.user_mapper.utable)
                .ok_or_else(|| {
                    self.mappings.insert_vma(cur_vma.clone());
                    SystemError::EFAULT
                })?;
            let (before, after) = (split_result.prev, split_result.after);

            {
                let mut guard = cur_vma.lock();
                guard.set_vm_flags(committed_flags);
            }

            if let Some(locked_vm_after) = locked_vm_after {
                self.locked_vm = locked_vm_after;
            }
            self.update_present_page_mlock_refs(
                &cur_vma,
                intersection.start(),
                intersection.end(),
                old_locked,
                wants_locked,
            );

            if let Some(before) = before {
                self.mappings.insert_vma(before);
            }
            if let Some(after) = after {
                self.mappings.insert_vma(after);
            }
            self.mappings.insert_vma(cur_vma);
            split_lifecycle.commit();
        }

        if wants_locked {
            let fault_in_missing = !new_flags.contains(VmFlags::VM_LOCKONFAULT);
            let result = self.populate_vma_range(target.start(), target.size(), fault_in_missing);
            if !ignore_populate_errors {
                result?;
            }
        } else {
            self.munlock_vma_pages_range(target.start(), target.end())?;
        }

        Ok(())
    }

    fn checked_user_region(start: VirtAddr, len: usize) -> Result<VirtRegion, SystemError> {
        if len == 0 {
            return Err(SystemError::EINVAL);
        }
        if !start.check_aligned(MMArch::PAGE_SIZE) {
            return Err(SystemError::EINVAL);
        }
        let end = start.data().checked_add(len).ok_or(SystemError::EINVAL)?;
        if end > MMArch::USER_END_VADDR.data() {
            return Err(SystemError::EINVAL);
        }
        Ok(VirtRegion::new(start, len))
    }

    fn munlock_vma_pages_range(
        &mut self,
        start: VirtAddr,
        end: VirtAddr,
    ) -> Result<(), SystemError> {
        let mapper = &self.user_mapper.utable;
        let mut vaddr = start;
        while vaddr < end {
            if let Some((paddr, _)) = mapper.translate(vaddr) {
                let page = {
                    let mut page_manager_guard = page_manager_lock();
                    page_manager_guard.get_unwrap(&paddr)
                };
                Self::remove_page_unevictable_if_unneeded(&page);
            }
            vaddr = VirtAddr::new(vaddr.data() + MMArch::PAGE_SIZE);
        }
        Ok(())
    }

    pub(crate) fn remove_page_unevictable_if_unneeded(page: &Arc<Page>) {
        let mut page_guard = page.write();
        if !page_guard.flags().contains(PageFlags::PG_UNEVICTABLE)
            || page_guard.has_unevictable_source()
        {
            return;
        }

        page_guard.remove_flags(PageFlags::PG_UNEVICTABLE);
        let paddr = page.phys_address();
        let should_reclaim = page_guard.flags().contains(PageFlags::PG_LRU);
        drop(page_guard);
        if should_reclaim {
            page_reclaimer_lock().insert_page(paddr, page);
        }
    }

    fn madvise_uses_range_without_vma_split(behavior: MadvFlags) -> bool {
        behavior == MadvFlags::MADV_DONTNEED
            || behavior == MadvFlags::MADV_DONTNEED_LOCKED
            || behavior == MadvFlags::MADV_WILLNEED
            || behavior == MadvFlags::MADV_COLD
            || behavior == MadvFlags::MADV_PAGEOUT
            || behavior == MadvFlags::MADV_FREE
            || behavior == MadvFlags::MADV_POPULATE_READ
            || behavior == MadvFlags::MADV_POPULATE_WRITE
    }

    pub fn madvise(
        &mut self,
        start_page: VirtPageFrame,
        page_count: PageFrameCount,
        behavior: MadvFlags,
    ) -> Result<(), SystemError> {
        let mm = self.outer_addr_space().ok_or(SystemError::EFAULT)?;
        let mut tlb = MmuGather::gather(&mm);

        let mapper = &mut self.user_mapper.utable;

        let region = VirtRegion::new(start_page.virt_address(), page_count.bytes());
        let (regions, has_unmapped) = self.mappings.conflicts_with_unmapped(region);

        if behavior == MadvFlags::MADV_DOFORK {
            for vma in &regions {
                if vma.lock().vm_flags().contains(VmFlags::VM_IO) {
                    return Err(SystemError::EINVAL);
                }
            }
        }
        if behavior == MadvFlags::MADV_REMOVE {
            return if regions.is_empty() {
                Err(SystemError::ENOMEM)
            } else {
                Err(SystemError::EINVAL)
            };
        }

        if Self::madvise_uses_range_without_vma_split(behavior) {
            for r in regions {
                let (original_region, vm_flags) = {
                    let guard = r.lock();
                    (*guard.region(), *guard.vm_flags())
                };
                let intersection = original_region.intersect(&region).unwrap();

                let _pt_edit = mm.page_table_edit();
                match behavior {
                    MadvFlags::MADV_DONTNEED | MadvFlags::MADV_DONTNEED_LOCKED => {
                        if vm_flags.contains(VmFlags::VM_PFNMAP)
                            || (behavior == MadvFlags::MADV_DONTNEED
                                && vm_flags.contains(VmFlags::VM_LOCKED))
                        {
                            tlb.finish();
                            return Err(SystemError::EINVAL);
                        }
                        r.unmap_range(intersection, mapper, &mut tlb, UnmapMappingMode::EvenCow);
                    }
                    _ => r.do_madvise(behavior, mapper, &mut tlb),
                }
            }
            tlb.finish();
            return if has_unmapped {
                Err(SystemError::ENOMEM)
            } else {
                Ok(())
            };
        }

        let mut plans = Vec::with_capacity(regions.len());
        for r in &regions {
            let (original_region, old_flags) = {
                let guard = r.lock();
                (*guard.region(), *guard.vm_flags())
            };
            let Some(new_flags) = r.madvise_updated_flags(behavior)? else {
                continue;
            };
            if new_flags == old_flags {
                continue;
            }
            let intersection = original_region.intersect(&region).unwrap();
            let split_lifecycle = r.prepare_split_lifecycle(intersection)?;
            plans.push(MadviseVmaPlan {
                original_region,
                intersection,
                split_lifecycle,
            });
        }

        for plan in plans {
            let r = match self.mappings.remove_vma(&plan.original_region) {
                Some(vma) => vma,
                None => return Err(SystemError::EFAULT),
            };

            let madvise_result: Result<VmaSplitSides, SystemError> = {
                let _pt_edit = mm.page_table_edit();
                let split_result = r
                    .extract(plan.intersection, mapper)
                    .expect("Failed to extract VMA");
                r.do_madvise(behavior, mapper, &mut tlb);
                Ok((split_result.prev, split_result.after))
            };
            let (before, after) = match madvise_result {
                Ok(result) => result,
                Err(err) => {
                    self.mappings.insert_vma(r);
                    return Err(err);
                }
            };
            if let Some(before) = before {
                self.mappings.insert_vma(before);
            }
            if let Some(after) = after {
                self.mappings.insert_vma(after);
            }
            self.mappings.insert_vma(r);
            plan.split_lifecycle.commit();
        }
        tlb.finish();
        if has_unmapped {
            Err(SystemError::ENOMEM)
        } else {
            Ok(())
        }
    }

    /// 取消与指定 inode 关联的文件映射的页表项，保留 VMA 以便后续访问触发缺页并按最新文件大小处理
    pub fn zap_file_mappings(&mut self, inode_id: InodeId) -> Result<(), SystemError> {
        let mut targets: Vec<Arc<LockedVMA>> = Vec::new();
        for vma in self.mappings.iter_vmas() {
            let guard = vma.lock();
            if let Some(file) = guard.vm_file() {
                if file.inode().metadata()?.inode_id == inode_id {
                    targets.push(vma.clone());
                }
            }
        }

        let mm = self.outer_addr_space().ok_or(SystemError::EFAULT)?;
        let _pt_edit = mm.page_table_edit();
        let mut tlb = MmuGather::gather(&mm);
        for vma in targets {
            vma.unmap(&mut self.user_mapper.utable, &mut tlb);
        }
        tlb.finish();
        Ok(())
    }

    /// 创建新的用户栈
    ///
    /// ## 参数
    ///
    /// - `size`：栈的大小
    pub fn new_user_stack(&mut self, size: usize) -> Result<(), SystemError> {
        assert!(self.user_stack.is_none(), "User stack already exists");
        let stack = UserStack::new(self, None, size)?;
        self.user_stack = Some(stack);
        return Ok(());
    }

    #[inline(always)]
    pub fn user_stack_mut(&mut self) -> Option<&mut UserStack> {
        return self.user_stack.as_mut();
    }

    /// 取消用户空间内的所有映射
    pub unsafe fn unmap_all(&mut self) {
        // 两种调用场景：
        // 1) 显式调用（仍有 `Arc<AddressSpace>` 外部引用）：`outer.upgrade()` 返回 `Some`，
        //    走正常的 mm-aware shootdown + 释放路径。
        // 2) `Drop for InnerAddressSpace`：此时 `Arc<AddressSpace>` 正处于 `drop_slow`，
        //    strong-count 已经是 0，`Weak::upgrade()` 必然返回 `None`。此路径下我们已经在
        //    exit/switch_process 里清理过 `active_cpus`，不会有任何 CPU 还持有这个 mm 的 TLB，
        //    因此使用 `MmuGather::gather_teardown()`，跳过跨核 shootdown，只把 PTE 拆掉、
        //    按 INV-3 的 "先 flush，后释放" 顺序释放物理页。
        let mm_arc = self.outer_addr_space();
        let _pt_edit = mm_arc.as_ref().map(|mm| mm.page_table_edit());
        let mut tlb = match mm_arc.as_ref() {
            Some(mm) => MmuGather::gather(mm),
            None => MmuGather::gather_teardown(),
        };
        // Full-mm flush (fullmm); no need to accumulate ranges.
        tlb.set_fullmm();
        let mut vma_close_notifications = Vec::new();
        let mut sysv_close_notifications = Vec::new();
        for vma in self.mappings.take_all_vmas() {
            let region = *vma.lock().region();
            if let Some(notification) = Self::collect_vma_close(&vma, region) {
                vma_close_notifications.push(notification);
            }
            let sysv_close = Self::collect_sysv_shm_close(&vma);
            if let Some(notification) = sysv_close {
                sysv_close_notifications.push(notification);
            }
            if vma.mapped() {
                vma.unmap(&mut self.user_mapper.utable, &mut tlb);
            }
        }
        tlb.finish();
        for notification in vma_close_notifications {
            Self::notify_vma_close(notification);
        }
        for notification in sysv_close_notifications {
            Self::notify_sysv_shm_close(notification);
        }
    }

    /// 设置进程的堆的内存空间
    ///
    /// ## 参数
    ///
    /// - `new_brk`：新的堆的结束地址。需要满足页对齐要求，并且是用户空间地址，且大于等于当前的堆的起始地址
    ///
    /// ## 返回值
    ///
    /// 返回旧的堆的结束地址
    pub unsafe fn set_brk(&mut self, new_brk: VirtAddr) -> Result<VirtAddr, SystemError> {
        assert!(new_brk.check_aligned(MMArch::PAGE_SIZE));

        if !new_brk.check_user() || new_brk < self.brk_start {
            return Err(SystemError::EFAULT);
        }

        // 软限制：RLIMIT_DATA
        let rlim = ProcessManager::current_pcb()
            .get_rlimit(RLimitID::Data)
            .rlim_cur as usize;
        if rlim != usize::MAX {
            let desired = new_brk.data().saturating_sub(self.brk_start.data());
            if desired > rlim {
                return Err(SystemError::ENOMEM);
            }
        }

        let old_brk = self.brk;

        if new_brk > self.brk {
            let len = new_brk - self.brk;
            let brk_region = VirtRegion::new(self.brk, len);
            if self.mappings.has_conflict(brk_region) {
                return Err(SystemError::ENOMEM);
            }
            let prot_flags = ProtFlags::PROT_READ | ProtFlags::PROT_WRITE;
            let map_flags =
                MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS | MapFlags::MAP_FIXED_NOREPLACE;
            self.map_anonymous(old_brk, len, prot_flags, map_flags, true, false)?;

            self.brk = new_brk;
            return Ok(old_brk);
        } else {
            let unmap_len = self.brk - new_brk;
            let unmap_start = new_brk;
            if unmap_len == 0 {
                return Ok(old_brk);
            }
            self.munmap(
                VirtPageFrame::new(unmap_start),
                PageFrameCount::from_bytes(unmap_len).unwrap(),
            )?;
            self.brk = new_brk;
            return Ok(old_brk);
        }
    }

    pub unsafe fn sbrk(&mut self, incr: isize) -> Result<VirtAddr, SystemError> {
        if incr == 0 {
            return Ok(self.brk);
        }

        let new_brk = if incr > 0 {
            self.brk + incr as usize
        } else {
            self.brk - incr.unsigned_abs()
        };

        let new_brk = VirtAddr::new(page_align_up(new_brk.data()));

        let rlim = ProcessManager::current_pcb()
            .get_rlimit(RLimitID::Data)
            .rlim_cur as usize;
        if rlim != usize::MAX {
            let desired = new_brk.data().saturating_sub(self.brk_start.data());
            if desired > rlim {
                return Err(SystemError::ENOMEM);
            }
        }

        return self.set_brk(new_brk);
    }

    fn find_free_at_prepare(
        &mut self,
        min_vaddr: VirtAddr,
        vaddr: VirtAddr,
        size: usize,
        flags: MapFlags,
    ) -> Result<VirtRegion, SystemError> {
        self.find_free_at_internal(min_vaddr, vaddr, size, flags, false)
            .map(|(region, _)| region)
    }

    fn find_free_at_collect(
        &mut self,
        min_vaddr: VirtAddr,
        vaddr: VirtAddr,
        size: usize,
        flags: MapFlags,
    ) -> Result<(VirtRegion, VmaCloseNotifications), SystemError> {
        self.find_free_at_internal(min_vaddr, vaddr, size, flags, true)
    }

    fn find_free_at_internal(
        &mut self,
        min_vaddr: VirtAddr,
        vaddr: VirtAddr,
        size: usize,
        flags: MapFlags,
        unmap_fixed: bool,
    ) -> Result<(VirtRegion, VmaCloseNotifications), SystemError> {
        // 如果没有指定地址，那么就在当前进程的地址空间中寻找一个空闲的虚拟内存范围。
        if vaddr == VirtAddr::new(0)
            && !flags.intersects(MapFlags::MAP_FIXED | MapFlags::MAP_FIXED_NOREPLACE)
        {
            let region = self
                .mappings
                .find_free(min_vaddr, size)
                .ok_or(SystemError::ENOMEM)?;
            return Ok((region, VmaCloseNotifications::default()));
        }

        let end = vaddr.data().checked_add(size).ok_or(SystemError::EINVAL)?;
        if size == 0
            || end > MMArch::USER_END_VADDR.data()
            || !vaddr.check_aligned(MMArch::PAGE_SIZE)
        {
            return Err(SystemError::EINVAL);
        }

        if vaddr < min_vaddr {
            if flags.intersects(MapFlags::MAP_FIXED | MapFlags::MAP_FIXED_NOREPLACE) {
                check_mmap_min_addr(vaddr, min_vaddr)?;
            } else {
                let region = self
                    .mappings
                    .find_free(min_vaddr, size)
                    .ok_or(SystemError::ENOMEM)?;
                return Ok((region, VmaCloseNotifications::default()));
            }
        }

        // 如果指定了地址，那么就检查指定的地址是否可用。
        let requested = VirtRegion::new(vaddr, size);

        if self
            .mappings
            .first_reservation_conflict(requested)
            .is_some()
        {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }

        let has_conflict = self.mappings.has_conflict(requested);
        if has_conflict {
            if flags.contains(MapFlags::MAP_FIXED_NOREPLACE) {
                // 如果指定了 MAP_FIXED_NOREPLACE 标志，由于所指定的地址无法成功建立映射，则放弃映射，不对地址做修正
                return Err(SystemError::EEXIST);
            }

            if flags.contains(MapFlags::MAP_FIXED) {
                if !unmap_fixed {
                    return Ok((requested, VmaCloseNotifications::default()));
                }
                // Linux mmap_region() unmaps the whole requested range for MAP_FIXED,
                // because the new mapping may overlap more than the first conflicting VMA.
                let notifications = self.munmap_collect(
                    VirtPageFrame::new(requested.start()),
                    PageFrameCount::from_bytes(requested.size()).unwrap(),
                )?;
                return Ok((requested, notifications));
            }

            // 如果没有指定MAP_FIXED标志，那么就对地址做修正
            let requested = self
                .mappings
                .find_free(min_vaddr, size)
                .ok_or(SystemError::ENOMEM)?;
            return Ok((requested, VmaCloseNotifications::default()));
        }

        return Ok((requested, VmaCloseNotifications::default()));
    }
}

impl Drop for InnerAddressSpace {
    fn drop(&mut self) {
        unsafe {
            self.unmap_all();
        }
        crate::mm::oom::notify_mm_drop(self.mm_id);
    }
}

#[derive(Debug, Hash)]
pub struct UserMapper {
    pub utable: PageMapper,
}

impl UserMapper {
    pub fn new(utable: PageMapper) -> Self {
        return Self { utable };
    }

    /// 拷贝用户空间映射
    /// ## 参数
    ///
    /// - `umapper`: 要拷贝的用户空间
    /// - `copy_on_write`: 是否写时复制
    pub unsafe fn clone_from(&mut self, umapper: &mut Self, copy_on_write: bool) {
        self.utable
            .clone_user_mapping(&mut umapper.utable, copy_on_write);
    }
}

impl Drop for UserMapper {
    fn drop(&mut self) {
        if self.utable.is_current() {
            // 如果当前要被销毁的用户空间的页表是当前进程的页表，那么就切换回初始内核页表
            unsafe { MMArch::set_table(PageTableKind::User, MMArch::initial_page_table()) }
        }
        // 释放用户空间顶层页表占用的页帧
        // 请注意，在释放这个页帧之前，用户页表应该已经被完全释放，否则会产生内存泄露
        unsafe {
            deallocate_page_frames(
                PhysPageFrame::new(self.utable.table().phys()),
                PageFrameCount::new(1),
            )
        };
    }
}

/// 用户空间映射信息
#[derive(Clone, Copy, Debug)]
struct MmapReservation {
    id: MmapReservationId,
    region: VirtRegion,
}

#[derive(Debug)]
pub struct UserMappings {
    /// 当前用户空间的虚拟内存区域
    vmas: HashSet<Arc<LockedVMA>>,
    /// 按起始地址索引的 VMA，用于地址查找、范围扫描和删除。
    vmas_by_start: BTreeMap<VirtAddr, Arc<LockedVMA>>,
    /// 当前用户空间的VMA空洞
    vm_holes: BTreeMap<VirtAddr, usize>,
    /// 正在建立、但尚未发布为 VMA 的 mmap 地址预约。
    reservations: BTreeMap<VirtAddr, MmapReservation>,
    /// 所属地址空间，用于在 VMA 生命周期变更时回填反向引用
    owner: Weak<AddressSpace>,
}

impl UserMappings {
    pub fn new() -> Self {
        return Self {
            vmas: HashSet::new(),
            vmas_by_start: BTreeMap::new(),
            vm_holes: core::iter::once((VirtAddr::new(0), MMArch::USER_END_VADDR.data()))
                .collect::<BTreeMap<_, _>>(),
            reservations: BTreeMap::new(),
            owner: Weak::new(),
        };
    }

    pub fn set_owner(&mut self, owner: Weak<AddressSpace>) {
        self.owner = owner;
        for vma in self.vmas.iter() {
            self.attach_vma(vma);
        }
    }

    fn attach_vma(&self, vma: &Arc<LockedVMA>) {
        let vm_file = {
            let mut guard = vma.lock();
            if guard.user_address_space.is_none() {
                guard.user_address_space = Some(self.owner.clone());
            }
            guard.vm_file.clone()
        };
        if let Some(file) = vm_file {
            if let Some(page_cache) = file.inode().page_cache() {
                page_cache.register_file_vma(vma);
            }
        }
    }

    fn detach_vma(&self, vma: &Arc<LockedVMA>) {
        let vm_file = { vma.lock().vm_file.clone() };
        if let Some(file) = vm_file {
            if let Some(page_cache) = file.inode().page_cache() {
                page_cache.unregister_file_vma(vma.id());
            }
        }
    }

    /// 判断当前进程的VMA内，是否有包含指定的虚拟地址的VMA。
    ///
    /// 如果有，返回包含指定虚拟地址的VMA的Arc指针，否则返回None。
    #[allow(dead_code)]
    pub fn contains(&self, vaddr: VirtAddr) -> Option<Arc<LockedVMA>> {
        let (_, vma) = self.vmas_by_start.range(..=vaddr).next_back()?;
        if vma.lock().region.contains(vaddr) {
            Some(vma.clone())
        } else {
            None
        }
    }

    /// 向下寻找距离虚拟地址最近的VMA
    /// ## 参数
    ///
    /// - `vaddr`: 虚拟地址
    ///
    /// ## 返回值
    /// - Some(Arc<LockedVMA>): 虚拟地址所在的或最近的下一个VMA
    /// - None: 未找到VMA
    #[allow(dead_code)]
    pub fn find_nearest(&self, vaddr: VirtAddr) -> Option<Arc<LockedVMA>> {
        if let Some(vma) = self.contains(vaddr) {
            return Some(vma);
        }
        self.vmas_by_start
            .range(vaddr..)
            .next()
            .map(|(_, vma)| vma.clone())
    }

    /// 获取当前进程的地址空间中，与给定虚拟地址范围有重叠的VMA。
    pub fn conflicts(&self, request: VirtRegion) -> Vec<Arc<LockedVMA>> {
        let mut result = Vec::new();
        if let Some((start, vma)) = self.vmas_by_start.range(..=request.start()).next_back() {
            if *start < request.start() && vma.lock().region.intersect(&request).is_some() {
                result.push(vma.clone());
            }
        }
        for (_start, vma) in self.vmas_by_start.range(request.start()..request.end()) {
            if vma.lock().region.intersect(&request).is_some() {
                result.push(vma.clone());
            }
        }
        result
    }

    pub fn has_conflict(&self, request: VirtRegion) -> bool {
        if let Some((start, vma)) = self.vmas_by_start.range(..=request.start()).next_back() {
            if *start < request.start() && vma.lock().region.intersect(&request).is_some() {
                return true;
            }
        }
        self.vmas_by_start
            .range(request.start()..request.end())
            .any(|(_start, vma)| vma.lock().region.intersect(&request).is_some())
    }

    pub fn conflicts_with_unmapped(&self, request: VirtRegion) -> (Vec<Arc<LockedVMA>>, bool) {
        let conflicts = self.conflicts(request);
        let mut cursor = request.start();
        let mut has_unmapped = false;

        for vma in &conflicts {
            let vma_region = *vma.lock().region();
            if vma_region.start() > cursor {
                has_unmapped = true;
            }
            if vma_region.end() > cursor {
                cursor = cmp::min(vma_region.end(), request.end());
            }
        }
        if cursor < request.end() {
            has_unmapped = true;
        }

        (conflicts, has_unmapped)
    }

    pub fn iter_vmas_starting_at(
        &self,
        start: VirtAddr,
    ) -> impl Iterator<Item = Arc<LockedVMA>> + '_ {
        self.vmas_by_start
            .range(start..)
            .map(|(_start, vma)| vma.clone())
    }

    pub fn first_reservation_conflict(&self, request: VirtRegion) -> Option<MmapReservationId> {
        self.reservations
            .values()
            .find(|reservation| reservation.region.collide(&request))
            .map(|reservation| reservation.id)
    }

    pub fn first_reservation_region(&self) -> Option<VirtRegion> {
        self.reservations
            .values()
            .next()
            .map(|reservation| reservation.region)
    }

    fn reservation_usage_bytes(&self) -> usize {
        self.reservations
            .values()
            .map(|reservation| reservation.region.size())
            .sum()
    }

    fn region_available_for_reservation(&self, region: VirtRegion) -> bool {
        !self.has_conflict(region) && self.first_reservation_conflict(region).is_none()
    }

    fn reserve_region(&mut self, region: VirtRegion) -> Result<MmapReservationId, SystemError> {
        if !self.region_available_for_reservation(region) {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }

        let id = MMAP_RESERVATION_ID_ALLOCATOR.fetch_add(1, Ordering::Relaxed);
        self.reserve_hole(&region);
        self.reservations
            .insert(region.start(), MmapReservation { id, region });
        Ok(id)
    }

    fn cancel_reservation(&mut self, id: MmapReservationId) -> Option<VirtRegion> {
        let start = self
            .reservations
            .iter()
            .find_map(|(start, reservation)| (reservation.id == id).then_some(*start))?;
        let reservation = self.reservations.remove(&start)?;
        self.unreserve_hole(&reservation.region);
        Some(reservation.region)
    }

    fn remove_reservation_for_commit(
        &mut self,
        id: MmapReservationId,
        region: VirtRegion,
    ) -> Result<(), SystemError> {
        let start = self
            .reservations
            .iter()
            .find_map(|(start, reservation)| (reservation.id == id).then_some(*start))
            .ok_or(SystemError::EFAULT)?;
        let reservation = *self.reservations.get(&start).ok_or(SystemError::EFAULT)?;
        if reservation.region != region {
            return Err(SystemError::EFAULT);
        }
        self.reservations.remove(&start);
        Ok(())
    }

    fn commit_reserved_vma(
        &mut self,
        id: MmapReservationId,
        vma: Arc<LockedVMA>,
    ) -> Result<(), SystemError> {
        let region = vma.lock().region;
        self.remove_reservation_for_commit(id, region)?;
        self.insert_vma(vma);
        Ok(())
    }

    /// 在当前进程的地址空间中，寻找第一个符合条件的空闲的虚拟内存范围。
    ///
    /// @param min_vaddr 最小的起始地址
    /// @param size 请求的大小
    ///
    /// @return 如果找到了，返回虚拟内存范围，否则返回None
    pub fn find_free(&self, min_vaddr: VirtAddr, req_size: usize) -> Option<VirtRegion> {
        let mut iter = self
            .vm_holes
            .iter()
            .skip_while(|(hole_vaddr, hole_size)| hole_vaddr.add(**hole_size) <= min_vaddr);

        let (hole_vaddr, _hole_size) = iter.find(|(hole_vaddr, hole_size)| {
            // 计算当前空洞的可用大小
            let available_size: usize =
                if hole_vaddr <= &&min_vaddr && min_vaddr <= hole_vaddr.add(**hole_size) {
                    **hole_size - (min_vaddr - **hole_vaddr)
                } else {
                    **hole_size
                };

            req_size <= available_size
        })?;

        // 返回恰好等于请求大小的区域，起始地址取空洞与下限的较大值。
        let region = VirtRegion::new(cmp::max(*hole_vaddr, min_vaddr), req_size);

        return Some(region);
    }

    /// 在当前进程的地址空间中，保留一个指定大小的区域，使得该区域不在空洞中。
    /// 该函数会修改vm_holes中的空洞信息。
    ///
    /// @param region 要保留的区域
    ///
    /// 请注意，在调用本函数之前，必须先确定region所在范围内没有VMA。
    fn reserve_hole(&mut self, region: &VirtRegion) {
        let prev_hole: Option<(&VirtAddr, &mut usize)> =
            self.vm_holes.range_mut(..=region.start()).next_back();

        if let Some((prev_hole_vaddr, prev_hole_size)) = prev_hole {
            let prev_hole_end = prev_hole_vaddr.add(*prev_hole_size);

            if prev_hole_end > region.start() {
                // 如果前一个空洞的结束地址大于当前空洞的起始地址，那么就需要调整前一个空洞的大小。
                *prev_hole_size = region.start().data() - prev_hole_vaddr.data();
            }

            if prev_hole_end > region.end() {
                // 如果前一个空洞的结束地址大于当前空洞的结束地址，那么就需要增加一个新的空洞。
                self.vm_holes
                    .insert(region.end(), prev_hole_end - region.end());
            }
        }
    }

    /// 在当前进程的地址空间中，释放一个指定大小的区域，使得该区域成为一个空洞。
    /// 该函数会修改vm_holes中的空洞信息。
    fn unreserve_hole(&mut self, region: &VirtRegion) {
        // 如果将要插入的空洞与后一个空洞相邻，那么就需要合并。
        let next_hole_size: Option<usize> = self.vm_holes.remove(&region.end());

        if let Some((_prev_hole_vaddr, prev_hole_size)) = self
            .vm_holes
            .range_mut(..region.start())
            .next_back()
            .filter(|(offset, size)| offset.data() + **size == region.start().data())
        {
            *prev_hole_size += region.size() + next_hole_size.unwrap_or(0);
        } else {
            self.vm_holes
                .insert(region.start(), region.size() + next_hole_size.unwrap_or(0));
        }
    }

    /// 在当前进程的映射关系中，插入一个新的VMA。
    pub fn insert_vma(&mut self, vma: Arc<LockedVMA>) {
        let region = vma.lock().region;
        // 要求插入的地址范围必须是空闲的，也就是说，当前进程的地址空间中，不能有任何与之重叠的VMA。
        assert!(!self.has_conflict(region));
        self.reserve_hole(&region);

        self.attach_vma(&vma);
        self.vmas_by_start.insert(region.start(), vma.clone());
        self.vmas.insert(vma);
    }

    /// 将一个 VMA 从当前Mapping中移除，并把对应的地址空间加入空洞中。
    ///
    /// 这里不会取消VMA对应的地址的映射，即不会修改进程页表
    ///
    /// ### 参数
    ///  region 要删除的VMA所在的地址范围
    ///
    /// ### 返回值
    /// - 如果成功删除了VMA，则返回被删除的VMA，否则返回None
    /// - 如果没有可以删除的VMA，则不会执行删除操作，并报告失败。
    ///
    /// ### 副作用
    /// - 会修改vm_holes中的空洞信息
    ///
    pub fn remove_vma(&mut self, region: &VirtRegion) -> Option<Arc<LockedVMA>> {
        let vma = self.vmas_by_start.remove(&region.start())?;
        if vma.lock().region != *region {
            self.vmas_by_start.insert(region.start(), vma);
            return None;
        }
        let removed = self.vmas.remove(&vma);
        debug_assert!(removed, "vmas_by_start and vmas diverged for {:?}", region);
        self.unreserve_hole(region);
        self.detach_vma(&vma);

        return Some(vma);
    }

    fn take_all_vmas(&mut self) -> Vec<Arc<LockedVMA>> {
        let vmas = self.vmas.drain().collect::<Vec<_>>();
        self.vmas_by_start.clear();
        self.reservations.clear();
        self.vm_holes.clear();
        self.vm_holes
            .insert(VirtAddr::new(0), MMArch::USER_END_VADDR.data());
        for vma in &vmas {
            self.detach_vma(vma);
        }
        vmas
    }

    /// @brief Get the iterator of all VMAs in this process.
    pub fn iter_vmas(&self) -> hashbrown::hash_set::Iter<'_, Arc<LockedVMA>> {
        return self.vmas.iter();
    }
}

impl Default for UserMappings {
    fn default() -> Self {
        return Self::new();
    }
}

/// 加了锁的VMA
///
/// 备注：进行性能测试，看看SpinLock和RwLock哪个更快。
#[derive(Debug)]
pub struct LockedVMA {
    /// 用于计算哈希值，避免总是获取vma锁来计算哈希值
    id: usize,
    state_seq: AtomicU64,
    vma: Mutex<VMA>,
}

impl core::hash::Hash for LockedVMA {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

impl PartialEq for LockedVMA {
    fn eq(&self, other: &Self) -> bool {
        self.id.eq(&other.id)
    }
}

impl Eq for LockedVMA {}

#[allow(dead_code)]
impl LockedVMA {
    pub fn new(vma: VMA) -> Arc<Self> {
        let r = Arc::new(Self {
            id: LOCKEDVMA_ID_ALLOCATOR.lock().alloc().unwrap(),
            state_seq: AtomicU64::new(0),
            vma: Mutex::new(vma),
        });
        r.vma.lock().self_ref = Arc::downgrade(&r);
        return r;
    }

    pub fn id(&self) -> usize {
        self.id
    }

    pub fn state_seq(&self) -> u64 {
        self.state_seq.load(Ordering::Acquire)
    }

    fn bump_state_seq(&self) {
        self.state_seq.fetch_add(1, Ordering::AcqRel);
    }

    pub fn lock(&self) -> MutexGuard<'_, VMA> {
        return self.vma.lock();
    }

    fn prepare_split_lifecycle(
        &self,
        intersection: VirtRegion,
    ) -> Result<VmaSplitLifecycle, SystemError> {
        let (original_region, sysv_shm) = {
            let guard = self.lock();
            if intersection == *guard.region() {
                return Ok(VmaSplitLifecycle::none());
            }
            (*guard.region(), guard.sysv_shm())
        };
        let Some(sysv_shm) = sysv_shm else {
            return Ok(VmaSplitLifecycle::none());
        };

        let mut lifecycle = VmaSplitLifecycle {
            sysv_shm: Some(sysv_shm.clone()),
            open_count: 0,
            committed: false,
        };
        if original_region.before(&intersection).is_some() {
            sysv_shm.open_vma()?;
            lifecycle.open_count += 1;
        }
        if original_region.after(&intersection).is_some() {
            if let Err(err) = sysv_shm.open_vma() {
                for _ in 0..lifecycle.open_count {
                    sysv_shm.close_vma();
                }
                lifecycle.open_count = 0;
                lifecycle.committed = true;
                return Err(err);
            }
            lifecycle.open_count += 1;
        }
        Ok(lifecycle)
    }

    /// 调整当前VMA的页面的标志位
    ///
    /// TODO：增加调整虚拟页映射的物理地址的功能
    ///
    /// @param flags 新的标志位
    /// @param mapper 页表映射器
    /// @param flusher 页表项刷新器
    ///
    pub fn remap(
        &self,
        flags: EntryFlags<MMArch>,
        mapper: &mut PageMapper,
        mut flusher: impl Flusher<MMArch>,
    ) -> Result<(), SystemError> {
        let mut guard = self.lock();
        for page in guard.region.pages() {
            if mapper.translate(page.virt_address()).is_some() {
                let r = unsafe {
                    mapper
                        .remap(page.virt_address(), flags)
                        .expect("Failed to remap")
                };
                flusher.consume(r);
            }
        }
        guard.flags = flags;
        return Ok(());
    }

    /// Unmap the entire VMA, stashing physical pages pending release into `tlb` and accumulating the TLB flush range.
    ///
    /// The caller must complete all PTE clears before calling `tlb.finish()`, which uniformly
    /// performs cross-core TLB shootdown first and then frees physical pages (INV-3).
    pub fn unmap(&self, mapper: &mut PageMapper, tlb: &mut MmuGather<'_>) {
        // todo: 如果当前vma与文件相关，完善文件相关的逻辑
        let (region, should_wakeup_writeback, mm) = {
            let mut self_guard = self.lock();
            let region = *self_guard.region();
            let mm = self_guard.address_space().and_then(|mm| mm.upgrade());
            self_guard.mapped = false;
            let should_wakeup_writeback = self_guard.vm_file().is_some()
                && self_guard
                    .vm_flags()
                    .contains(VmFlags::VM_SHARED | VmFlags::VM_WRITE);
            (region, should_wakeup_writeback, mm)
        };

        let mut pages_to_reclassify = Vec::new();
        let mut unmapped_present_pages = 0usize;
        {
            let mut page_manager_guard = page_manager_lock();
            for page in region.pages() {
                if mapper.translate(page.virt_address()).is_none() {
                    continue;
                }
                let (paddr, _, flush, freed_tables) =
                    unsafe { mapper.unmap_phys_with_freed_tables(page.virt_address(), true) }
                        .expect("Failed to unmap, beacuse of some page is not mapped");

                // 从anon_vma中删除当前VMA
                let page_arc = page_manager_guard.get_unwrap(&paddr);
                {
                    let mut page_guard = page_arc.write();
                    page_guard.remove_vma(self);
                }
                pages_to_reclassify.push((paddr, page_arc));

                // Local PTE cleared; no immediate invlpg. Final TLB invalidation is performed uniformly by MmuGather.
                unsafe { flush.ignore() };
                tlb.accumulate_range(page.virt_address());
                unmapped_present_pages += 1;
                if freed_tables {
                    tlb.note_pt_table_freed();
                }
            }
        }

        for (_, page_arc) in &pages_to_reclassify {
            InnerAddressSpace::remove_page_unevictable_if_unneeded(page_arc);
        }

        if let Some(mm) = mm.as_ref() {
            mm.account_present_pages_sub(unmapped_present_pages);
        }

        let mut page_manager_guard = page_manager_lock();
        for (paddr, page_arc) in pages_to_reclassify {
            // The physical page's VMA list length is 0 and it is not marked as non-reclaimable, so it can be freed.
            // TODO: LRU-based physical page reclamation in the future
            let can_dealloc = page_arc.read().can_deallocate_after_vma_unmap();

            if can_dealloc {
                // Remove this `Arc<Page>` from page_manager, deferring the drop until after TLB shootdown.
                // This guarantees INV-3: other cores can no longer reach the returned-to-buddy physical page via stale TLB.
                if let Some(p) = page_manager_guard.remove_page(&paddr) {
                    tlb.stash_page(p);
                }
            }
        }

        // 当vma对应共享文件的写映射时，唤醒脏页回写线程
        if should_wakeup_writeback {
            crate::mm::page::PageReclaimer::wakeup_claim_thread();
        }
    }

    /// Unmap an intersecting sub-range of this VMA while keeping the VMA metadata itself alive.
    ///
    /// This is used by file truncate/invalidate paths: future access should fault back in against
    /// the updated file size/content instead of tearing down the VMA object.
    pub fn unmap_range(
        &self,
        region: VirtRegion,
        mapper: &PageMapper,
        tlb: &mut MmuGather<'_>,
        mode: UnmapMappingMode,
    ) {
        let self_guard = self.lock();
        let Some(intersection) = self_guard.region().intersect(&region) else {
            return;
        };
        let mm = self_guard.address_space().and_then(|mm| mm.upgrade());
        let vma_start = self_guard.region().start();
        let backing_pgoff = self_guard.backing_page_offset();
        let file_page_cache = self_guard
            .vm_file()
            .and_then(|file| file.inode().page_cache());
        drop(self_guard);

        let mut pages_to_reclassify = Vec::new();
        let mut unmapped_present_pages = 0usize;
        {
            let mut page_manager_guard = page_manager_lock();
            for page in intersection.pages() {
                let virt = page.virt_address();
                let Some((paddr, _)) = mapper.translate(virt) else {
                    continue;
                };

                let page_arc = page_manager_guard.get_unwrap(&paddr);
                if let Some(page_cache) = file_page_cache.as_ref() {
                    let Some(base_pgoff) = backing_pgoff else {
                        continue;
                    };
                    let pgoff =
                        base_pgoff + ((virt.data() - vma_start.data()) >> MMArch::PAGE_SHIFT);
                    let page_guard = page_arc.read();
                    let is_target_page = match page_guard.page_type() {
                        PageType::File(info) if info.index == pgoff => info
                            .page_cache
                            .upgrade()
                            .is_some_and(|mapped_cache| Arc::ptr_eq(&mapped_cache, page_cache)),
                        // Truncate must also zap private COW pages. For file VMAs those pages are
                        // represented as normal pages, while shared file mappings remain page-cache
                        // backed and are covered by the PageType::File branch above.
                        PageType::Normal if mode == UnmapMappingMode::EvenCow => true,
                        _ => false,
                    };
                    drop(page_guard);
                    if !is_target_page {
                        continue;
                    }
                }

                let Some((paddr, _, flush)) = (unsafe { mapper.unmap_phys_preserve_tables(virt) })
                else {
                    continue;
                };

                {
                    let mut page_guard = page_arc.write();
                    page_guard.remove_vma(self);
                }
                pages_to_reclassify.push((paddr, page_arc));

                unsafe { flush.ignore() };
                tlb.accumulate_range(virt);
                unmapped_present_pages += 1;
            }
        }

        for (_, page_arc) in &pages_to_reclassify {
            InnerAddressSpace::remove_page_unevictable_if_unneeded(page_arc);
        }

        if let Some(mm) = mm.as_ref() {
            mm.account_present_pages_sub(unmapped_present_pages);
        }

        let mut page_manager_guard = page_manager_lock();
        for (paddr, page_arc) in pages_to_reclassify {
            let can_dealloc = page_arc.read().can_deallocate_after_vma_unmap();
            if can_dealloc {
                if let Some(p) = page_manager_guard.remove_page(&paddr) {
                    tlb.stash_page(p);
                }
            }
        }
    }

    pub fn mapped(&self) -> bool {
        return self.vma.lock().mapped;
    }

    /// 将当前 VMA 切分为最多三段（before / middle / after）。
    ///
    /// ### 参数
    /// - `region`：目标切分区域，必须页对齐，且**必须完全落在**当前 VMA 的范围内。
    /// - `utable`：用于查询虚拟页到物理页的映射，以便更新页的反向映射（anon_vma）。
    ///
    /// ### 返回值
    /// - `Some(VMASplitResult)`：切分成功。
    ///   - `prev`：位于 `region` 之前的 VMA（可能为 `None`）。
    ///   - `middle`：与 `region` 对应的 VMA（原 VMA 本体被收缩为此段）。
    ///   - `after`：位于 `region` 之后的 VMA（可能为 `None`）。
    /// - `None`：`region` 不合法（未完全包含于当前 VMA，或无法形成交集）。
    ///
    /// ### ⚠️ 关键副作用
    /// - **`self` 会被原地修改为 `middle`**：当前 VMA（即 `self`）的 `region` 会被修改为传入的 `region` 参数，
    ///   同时其 `backing_pgoff` 也会相应调整。**返回的 `middle` 与 `self` 是同一个 VMA**（通过 `Arc` 指向同一实例），
    ///   修改后 `self` 就是 `middle`。这是原地修改，不是创建新 VMA。
    /// - 可能创建新的 VMA（`before`/`after`），但它们初始为未映射状态。
    /// - 会更新 `before`/`after` 所覆盖页的反向映射（anon_vma），并从原 VMA 中移除。
    ///
    /// ### 复杂/隐式逻辑说明
    /// - `before/after` 的 `backing_pgoff` 调整：
    ///   `after` 需要偏移到原 VMA 中相应的页偏移；`before` 保持原始偏移。
    /// - 反向映射更新的原因：
    ///   VMA 切分后，物理页应归属到新的 VMA（`before`/`after`），否则页回收/共享判断会错误。
    /// - 当 `region` 与 VMA 完全一致时，直接返回当前 VMA，避免无意义切分。
    pub fn extract(&self, region: VirtRegion, utable: &PageMapper) -> Option<VMASplitResult> {
        assert!(region.start().check_aligned(MMArch::PAGE_SIZE));
        assert!(region.end().check_aligned(MMArch::PAGE_SIZE));

        let mut guard = self.lock();

        // ============================================================
        // 提前检查：处理三种无需切分的边界情况
        // ============================================================
        // 这个代码块用于处理三种特殊情况，在这些情况下无需进行 VMA 切分操作：
        // 1. region 跨越 VMA 的下边界或上边界 → 返回 None
        // 2. region 与当前 VMA 完全不相交 → 返回 None
        // 3. region 与当前 VMA 完全相等 → 直接返回当前 VMA，无需切分
        {
            // 如果传入的 region 跨越 VMA 的下边界或上边界，则返回 None，表示无法切分。
            // 因此使用 `region.start() < vma.start || region.end() > vma.end` 判错是刻意的，
            // 不是常见的“不相交判断”(region.end <= vma.start || region.start >= vma.end)。
            // 这样可以保证 `before/after/middle` 三段始终是原 VMA 的严格切分。
            if unlikely(region.start() < guard.region.start() || region.end() > guard.region.end())
            {
                return None;
            }
            let intersect: Option<VirtRegion> = guard.region.intersect(&region);

            // 如果当前 VMA.region 与 region 不相交，则直接返回None
            if unlikely(intersect.is_none()) {
                return None;
            }
            let intersect: VirtRegion = intersect.unwrap();

            // 如果当前 VMA.region 完全等于传入的 region，则无需切分，直接返回当前 VMA。
            if unlikely(intersect == guard.region) {
                return Some(VMASplitResult::new(
                    None,
                    guard.self_ref.upgrade().unwrap(),
                    None,
                ));
            }
        }

        let before: Option<Arc<LockedVMA>> = guard.region.before(&region).map(|virt_region| {
            let mut vma: VMA = unsafe { guard.clone() };
            vma.region = virt_region;
            vma.mapped = false;
            // backing_pgoff 保持不变，before VMA 使用原始的offset
            let vma: Arc<LockedVMA> = LockedVMA::new(vma);
            vma
        });

        let after: Option<Arc<LockedVMA>> = guard.region.after(&region).map(|virt_region| {
            let mut vma: VMA = unsafe { guard.clone() };
            vma.region = virt_region;
            vma.mapped = false;
            // after VMA 需要调整backing_pgoff
            // after 区域的起始地址相对于原始VMA起始地址的偏移（以页为单位）
            if let Some(original_pgoff) = vma.backing_pgoff {
                let offset_pages =
                    (virt_region.start() - guard.region.start()) >> MMArch::PAGE_SHIFT;
                vma.backing_pgoff = Some(original_pgoff + offset_pages);
            }
            let vma: Arc<LockedVMA> = LockedVMA::new(vma);
            vma
        });

        let vma_mlocked = guard.vm_flags().contains(VmFlags::VM_LOCKED);
        // 重新设置before、after这两个VMA里面的物理页的anon_vma
        let mut page_manager_guard = page_manager_lock();
        if let Some(before) = before.clone() {
            let virt_iter = before.lock().region.iter_pages();
            for frame in virt_iter {
                if let Some((paddr, _)) = utable.translate(frame.virt_address()) {
                    let page = page_manager_guard.get_unwrap(&paddr);
                    let mut page_guard = page.write();
                    page_guard.insert_vma(before.clone(), vma_mlocked);
                    page_guard.remove_vma(self);
                    before.lock().mapped = true;
                }
            }
        }
        if let Some(after) = after.clone() {
            let virt_iter = after.lock().region.iter_pages();
            for frame in virt_iter {
                if let Some((paddr, _)) = utable.translate(frame.virt_address()) {
                    let page = page_manager_guard.get_unwrap(&paddr);
                    let mut page_guard = page.write();
                    page_guard.insert_vma(after.clone(), vma_mlocked);
                    page_guard.remove_vma(self);
                    after.lock().mapped = true;
                }
            }
        }

        // 调整 middleVMA 的 region 和 backing_pgoff
        let original_start = guard.region.start();
        guard.region = region;
        if let Some(original_pgoff) = guard.backing_pgoff {
            let offset_pages = (region.start() - original_start) >> MMArch::PAGE_SHIFT;
            guard.backing_pgoff = Some(original_pgoff + offset_pages);
        }

        return Some(VMASplitResult::new(
            before,
            guard.self_ref.upgrade().unwrap(),
            after,
        ));
    }

    /// 判断VMA是否为外部（非当前进程空间）的VMA
    pub fn is_foreign(&self) -> bool {
        let guard = self.lock();
        if let Some(space) = guard.user_address_space.clone() {
            if let Some(space) = space.upgrade() {
                return AddressSpace::is_current(&space);
            } else {
                return true;
            }
        } else {
            return true;
        }
    }

    /// 判断VMA是否可访问
    pub fn is_accessible(&self) -> bool {
        let guard = self.lock();
        let vm_access_flags: VmFlags = VmFlags::VM_READ | VmFlags::VM_WRITE | VmFlags::VM_EXEC;
        guard.vm_flags().intersects(vm_access_flags)
    }

    /// 判断VMA是否为匿名映射
    pub fn is_anonymous(&self) -> bool {
        let guard = self.lock();
        guard.vm_file.is_none()
    }

    /// 判断VMA是否为大页映射
    pub fn is_hugepage(&self) -> bool {
        //TODO: 实现巨页映射判断逻辑，目前不支持巨页映射
        false
    }

    pub fn file_pgoff_intersection(
        &self,
        start_page_index: usize,
        end_page_index_exclusive: Option<usize>,
    ) -> Option<VirtRegion> {
        let guard = self.lock();
        let vma_pgoff = guard.backing_page_offset()?;
        let vma_pages = guard.region().size() >> MMArch::PAGE_SHIFT;
        let vma_end = vma_pgoff.saturating_add(vma_pages);
        let intersect_start = core::cmp::max(start_page_index, vma_pgoff);
        let intersect_end = match end_page_index_exclusive {
            Some(end) => core::cmp::min(end, vma_end),
            None => vma_end,
        };
        if intersect_start >= intersect_end {
            return None;
        }

        let offset_pages = intersect_start - vma_pgoff;
        let start = guard.region().start() + (offset_pages << MMArch::PAGE_SHIFT);
        Some(VirtRegion::new(
            start,
            (intersect_end - intersect_start) << MMArch::PAGE_SHIFT,
        ))
    }
}

impl Drop for LockedVMA {
    fn drop(&mut self) {
        LOCKEDVMA_ID_ALLOCATOR.lock().free(self.id);
    }
}

/// VMA切分结果
#[allow(dead_code)]
pub struct VMASplitResult {
    pub prev: Option<Arc<LockedVMA>>,
    pub middle: Arc<LockedVMA>,
    pub after: Option<Arc<LockedVMA>>,
}

type VmaSplitSides = (Option<Arc<LockedVMA>>, Option<Arc<LockedVMA>>);

impl VMASplitResult {
    pub fn new(
        prev: Option<Arc<LockedVMA>>,
        middle: Arc<LockedVMA>,
        post: Option<Arc<LockedVMA>>,
    ) -> Self {
        Self {
            prev,
            middle,
            after: post,
        }
    }
}

/// Parameters for physmap operation
#[derive(Debug)]
pub struct PhysmapParams {
    pub phys: PhysPageFrame,
    pub destination: VirtPageFrame,
    pub count: PageFrameCount,
    pub vm_flags: VmFlags,
    pub flags: EntryFlags<MMArch>,
}

/// @brief 虚拟内存区域
#[derive(Debug)]
pub struct VMA {
    /// 虚拟内存区域对应的虚拟地址范围
    region: VirtRegion,
    /// 虚拟内存区域标志
    vm_flags: VmFlags,
    /// VMA内的页帧的标志
    flags: EntryFlags<MMArch>,
    /// VMA内的页帧是否已经映射到页表
    mapped: bool,
    /// VMA所属的用户地址空间
    user_address_space: Option<Weak<AddressSpace>>,
    self_ref: Weak<LockedVMA>,

    vm_file: Option<Arc<File>>,
    /// VMA映射的后备对象(文件/共享匿名)相对于整个后备对象的偏移页数
    backing_pgoff: Option<usize>,

    provider: Provider,
    /// SysV SHM attach 身份，用于 Linux 风格 VMA open/close 生命周期。
    sysv_shm: Option<Arc<SysVShmAttach>>,
    /// 共享匿名映射的稳定身份（用于跨进程共享 futex key）
    pub(crate) shared_anon: Option<Arc<AnonSharedMapping>>,
}

impl core::hash::Hash for VMA {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.region.hash(state);
        self.flags.hash(state);
        self.mapped.hash(state);
    }
}

/// 描述不同类型的内存提供者或资源
#[derive(Debug)]
pub enum Provider {
    Allocated, // TODO:其他
}

/// 共享匿名映射的稳定身份
#[derive(Debug)]
pub struct AnonSharedMapping {
    pub id: u64,
    /// Fixed backing size in pages, established at creation time.
    /// Linux semantics: mremap() expanding a MAP_SHARED|MAP_ANONYMOUS mapping does not grow the
    /// underlying shmem object; access beyond this size should SIGBUS.
    size_pages: usize,
    // Per-page cache keyed by page index within the backing object; store physical address.
    pages: SpinLock<HashMap<usize, PhysAddr>>,
}

impl AnonSharedMapping {
    fn new_id() -> u64 {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        return NEXT_ID.fetch_add(1, Ordering::Relaxed);
    }

    pub fn new(size_pages: usize) -> Arc<Self> {
        Arc::new(Self {
            id: Self::new_id(),
            size_pages,
            pages: SpinLock::new(HashMap::new()),
        })
    }

    #[inline(always)]
    pub fn size_pages(&self) -> usize {
        self.size_pages
    }

    /// Get or create a shared page for the given offset atomically.
    /// This prevents the double-allocation race when multiple processes fault the same page.
    pub fn get_or_create_page(&self, pgoff: usize) -> Result<Arc<Page>, SystemError> {
        let mut guard = self.pages.lock_irqsave();
        if let Some(paddr) = guard.get(&pgoff).copied() {
            let mut pm = page_manager_lock();
            return Ok(pm.get_unwrap(&paddr));
        }

        // Allocate while holding the map lock to avoid duplicate creations.
        let mut pm = page_manager_lock();
        let mut allocator = LockedFrameAllocator;
        let page = pm.create_one_page(PageType::Normal, PageFlags::empty(), &mut allocator)?;
        page.write().add_backing_lifetime_pin();
        guard.insert(pgoff, page.phys_address());
        Ok(page)
    }
}

impl Drop for AnonSharedMapping {
    fn drop(&mut self) {
        // When the backing object is destroyed, allow cached pages to be freed.
        let pages: alloc::vec::Vec<PhysAddr> = {
            let guard = self.pages.lock_irqsave();
            guard.values().copied().collect()
        };

        let mut pm = page_manager_lock();
        for paddr in pages {
            if let Some(page) = pm.get(&paddr) {
                let mut pg = page.write();
                pg.remove_backing_lifetime_pin();
                if pg.can_deallocate() {
                    drop(pg);
                    pm.remove_page(&paddr);
                }
            }
        }
    }
}

#[allow(dead_code)]
impl VMA {
    pub fn new(
        region: VirtRegion,
        vm_flags: VmFlags,
        flags: EntryFlags<MMArch>,
        file: Option<Arc<File>>,
        pgoff: Option<usize>,
        mapped: bool,
    ) -> Self {
        VMA {
            region,
            vm_flags,
            flags,
            mapped,
            user_address_space: None,
            self_ref: Weak::default(),
            provider: Provider::Allocated,
            vm_file: file,
            backing_pgoff: pgoff,
            sysv_shm: None,
            shared_anon: None,
        }
    }

    pub fn region(&self) -> &VirtRegion {
        return &self.region;
    }

    pub fn vm_flags(&self) -> &VmFlags {
        return &self.vm_flags;
    }

    pub fn vm_file(&self) -> Option<Arc<File>> {
        return self.vm_file.clone();
    }

    pub fn address_space(&self) -> Option<Weak<AddressSpace>> {
        return self.user_address_space.clone();
    }

    pub fn set_vm_flags(&mut self, vm_flags: VmFlags) {
        let changed = self.vm_flags != vm_flags;
        self.vm_flags = vm_flags;
        if changed {
            if let Some(vma) = self.self_ref.upgrade() {
                vma.bump_state_seq();
            }
        }
    }

    pub fn set_region_size(&mut self, new_region_size: usize) {
        self.region.set_size(new_region_size);
    }

    pub fn set_mapped(&mut self, mapped: bool) {
        self.mapped = mapped;
    }

    pub fn set_flags(&mut self) {
        self.flags = MMArch::vm_get_page_prot(self.vm_flags);
    }

    #[inline(always)]
    pub fn set_sysv_shm(&mut self, sysv_shm: Option<Arc<SysVShmAttach>>) {
        self.sysv_shm = sysv_shm;
    }

    #[inline(always)]
    pub fn sysv_shm(&self) -> Option<Arc<SysVShmAttach>> {
        self.sysv_shm.clone()
    }

    /// # 拷贝当前VMA的内容
    ///
    /// ### 安全性
    ///
    /// 由于这样操作可能由于错误的拷贝，导致内存泄露、内存重复释放等问题，所以需要小心使用。
    pub unsafe fn clone(&self) -> Self {
        return Self {
            region: self.region,
            vm_flags: self.vm_flags,
            flags: self.flags,
            mapped: self.mapped,
            user_address_space: self.user_address_space.clone(),
            self_ref: self.self_ref.clone(),
            provider: Provider::Allocated,
            backing_pgoff: self.backing_pgoff,
            vm_file: self.vm_file.clone(),
            sysv_shm: self.sysv_shm.clone(),
            shared_anon: self.shared_anon.clone(),
        };
    }

    pub fn clone_info_only(&self) -> Self {
        return Self {
            region: self.region,
            vm_flags: self.vm_flags,
            flags: self.flags,
            mapped: self.mapped,
            user_address_space: None,
            self_ref: Weak::default(),
            provider: Provider::Allocated,
            backing_pgoff: self.backing_pgoff,
            vm_file: self.vm_file.clone(),
            sysv_shm: self.sysv_shm.clone(),
            shared_anon: self.shared_anon.clone(),
        };
    }

    #[inline(always)]
    pub fn flags(&self) -> EntryFlags<MMArch> {
        return self.flags;
    }

    #[inline(always)]
    pub fn backing_page_offset(&self) -> Option<usize> {
        return self.backing_pgoff;
    }

    pub fn pages(&self) -> VirtPageFrameIter {
        return VirtPageFrameIter::new(
            VirtPageFrame::new(self.region.start()),
            VirtPageFrame::new(self.region.end()),
        );
    }

    /// Modify the flags of all existing PTEs covered by this VMA (without changing physical page ownership).
    ///
    /// Does not immediately perform TLB invalidation; the range is accumulated into the passed `MmuGather`,
    /// and the upper layer performs unified shootdown after all modifications are complete
    /// (TLB first, then free/modify, guaranteeing INV-3).
    pub fn remap(
        &mut self,
        flags: EntryFlags<MMArch>,
        mapper: &mut PageMapper,
        tlb: &mut MmuGather<'_>,
    ) {
        let pte_flags = if self.vm_file.is_some()
            && self
                .vm_flags
                .contains(VmFlags::VM_SHARED | VmFlags::VM_WRITE)
        {
            flags.set_write(false)
        } else {
            flags
        };
        for page in self.region.pages() {
            // debug!("remap page {:?}", page.virt_address());
            if mapper.translate(page.virt_address()).is_some() {
                let r = unsafe {
                    mapper
                        .remap(page.virt_address(), pte_flags)
                        .expect("Failed to remap")
                };
                unsafe { r.ignore() };
                tlb.accumulate_range(page.virt_address());
            }
            // debug!("consume page {:?}", page.virt_address());
            // debug!("remap page {:?} done", page.virt_address());
        }
        self.flags = flags;
    }

    /// 检查当前VMA是否可以拥有指定的标志位
    ///
    /// ## 参数
    ///
    /// - `prot_flags` 要检查的标志位
    pub fn can_have_flags(&self, prot_flags: ProtFlags) -> bool {
        if prot_flags.contains(ProtFlags::PROT_READ) && !self.vm_flags.contains(VmFlags::VM_MAYREAD)
        {
            return false;
        }
        if prot_flags.contains(ProtFlags::PROT_WRITE)
            && !self.vm_flags.contains(VmFlags::VM_MAYWRITE)
        {
            return false;
        }
        if prot_flags.contains(ProtFlags::PROT_EXEC) && !self.vm_flags.contains(VmFlags::VM_MAYEXEC)
        {
            return false;
        }

        let is_downgrade = (self.flags.has_write() || !prot_flags.contains(ProtFlags::PROT_WRITE))
            && (self.flags.has_execute() || !prot_flags.contains(ProtFlags::PROT_EXEC));

        #[allow(clippy::unneeded_struct_pattern)]
        match self.provider {
            Provider::Allocated { .. } => true,

            #[allow(unreachable_patterns)]
            _ => is_downgrade,
        }
    }

    /// 把物理地址映射到虚拟地址
    ///
    /// @param params 物理映射参数
    /// @param mapper 页表映射器
    /// @param flusher 页表项刷新器
    ///
    /// @return 返回映射后的虚拟内存区域
    pub fn physmap(
        params: PhysmapParams,
        mapper: &mut PageMapper,
        mut flusher: impl Flusher<MMArch>,
    ) -> Result<Arc<LockedVMA>, SystemError> {
        let mut cur_phy = params.phys;
        let mut cur_dest = params.destination;

        for _ in 0..params.count.data() {
            // 将物理页帧映射到虚拟页帧
            let r = unsafe {
                mapper.map_phys(
                    cur_dest.virt_address(),
                    cur_phy.phys_address(),
                    params.flags,
                )
            }
            .expect("Failed to map phys, may be OOM error");

            // todo: 增加OOM处理

            // 刷新TLB
            flusher.consume(r);

            cur_phy = cur_phy.next();
            cur_dest = cur_dest.next();
        }

        let r: Arc<LockedVMA> = LockedVMA::new(VMA::new(
            VirtRegion::new(
                params.destination.virt_address(),
                params.count.data() * MMArch::PAGE_SIZE,
            ),
            params.vm_flags,
            params.flags,
            None,
            None,
            true,
        ));
        // 将VMA加入到anon_vma中
        let mut page_manager_guard = page_manager_lock();
        cur_phy = params.phys;
        let vma_mlocked = params.vm_flags.contains(VmFlags::VM_LOCKED);
        for _ in 0..params.count.data() {
            let paddr = cur_phy.phys_address();
            let page = page_manager_guard.get_unwrap(&paddr);
            page.write().insert_vma(r.clone(), vma_mlocked);
            cur_phy = cur_phy.next();
        }

        return Ok(r);
    }

    /// 从页分配器中分配一些物理页，并把它们映射到指定的虚拟地址，然后创建VMA
    /// ## 参数
    ///
    /// - `destination`: 要映射到的虚拟地址
    /// - `page_count`: 要映射的页帧数量
    /// - `vm_flags`: VMA标志位
    /// - `flags`: 页面标志位
    /// - `mapper`: 页表映射器
    /// - `flusher`: 页表项刷新器
    /// - `file`: 映射文件
    /// - `pgoff`: 返回映射后的虚拟内存区域
    ///
    /// ## 返回值
    /// - 页面错误处理信息标志
    #[allow(clippy::too_many_arguments)]
    pub fn zeroed(
        destination: VirtPageFrame,
        page_count: PageFrameCount,
        vm_flags: VmFlags,
        flags: EntryFlags<MMArch>,
        mapper: &mut PageMapper,
        mut flusher: impl Flusher<MMArch>,
        file: Option<Arc<File>>,
        pgoff: Option<usize>,
    ) -> Result<Arc<LockedVMA>, SystemError> {
        let mut cur_dest: VirtPageFrame = destination;
        let mut mapped_pages = Vec::new();
        // debug!(
        //     "VMA::zeroed: page_count = {:?}, destination={destination:?}",
        //     page_count
        // );
        for _ in 0..page_count.data() {
            // debug!(
            //     "VMA::zeroed: cur_dest={cur_dest:?}, vaddr = {:?}",
            //     cur_dest.virt_address()
            // );
            let Some(r) = (unsafe { mapper.map(cur_dest.virt_address(), flags) }) else {
                let mut page_manager_guard = page_manager_lock();
                for mapped in mapped_pages.into_iter().rev() {
                    if let Some((paddr, _flags, flush, _freed_tables)) =
                        unsafe { mapper.unmap_phys_with_freed_tables(mapped, true) }
                    {
                        flusher.consume(flush);
                        let _ = page_manager_guard.remove_page(&paddr);
                    }
                }
                return Err(SystemError::ENOMEM);
            };

            // 稍后再刷新TLB，这里取消刷新
            flusher.consume(r);
            mapped_pages.push(cur_dest.virt_address());
            cur_dest = cur_dest.next();
        }
        let r = LockedVMA::new(VMA::new(
            VirtRegion::new(
                destination.virt_address(),
                page_count.data() * MMArch::PAGE_SIZE,
            ),
            vm_flags,
            flags,
            file,
            pgoff,
            true,
        ));
        drop(flusher);
        // debug!("VMA::zeroed: flusher dropped");

        // 清空这些内存并将VMA加入到anon_vma中
        let mut page_manager_guard = page_manager_lock();
        let virt_iter: VirtPageFrameIter =
            VirtPageFrameIter::new(destination, destination.add(page_count));
        let vma_mlocked = vm_flags.contains(VmFlags::VM_LOCKED);
        for frame in virt_iter {
            let paddr = mapper.translate(frame.virt_address()).unwrap().0;

            // 将VMA加入到anon_vma
            let page = page_manager_guard.get_unwrap(&paddr);
            page.write().insert_vma(r.clone(), vma_mlocked);
        }
        // debug!("VMA::zeroed: done");
        return Ok(r);
    }

    pub fn page_address(&self, index: usize) -> Result<VirtAddr, SystemError> {
        if index >= self.backing_pgoff.unwrap() {
            let address =
                self.region.start + ((index - self.backing_pgoff.unwrap()) << MMArch::PAGE_SHIFT);
            if address <= self.region.end() {
                return Ok(address);
            }
        }
        return Err(SystemError::EFAULT);
    }
}

impl Drop for VMA {
    fn drop(&mut self) {
        // 当VMA被释放时，需要确保它已经被从页表中解除映射
        assert!(!self.mapped, "VMA is still mapped");
    }
}

impl PartialEq for VMA {
    fn eq(&self, other: &Self) -> bool {
        return self.region == other.region;
    }
}

impl Eq for VMA {}

impl PartialOrd for VMA {
    fn partial_cmp(&self, other: &Self) -> Option<cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for VMA {
    fn cmp(&self, other: &Self) -> cmp::Ordering {
        return self.region.cmp(&other.region);
    }
}

#[derive(Debug)]
pub struct UserStack {
    // 栈底地址
    stack_bottom: VirtAddr,
    // 当前已映射的大小
    mapped_size: usize,
    /// 栈顶地址（这个值需要仔细确定！因为它可能不会实时与用户栈的真实栈顶保持一致！要小心！）
    current_sp: VirtAddr,
    /// 用户自定义的栈大小限制
    max_limit: usize,
}

impl UserStack {
    /// 默认的用户栈底地址
    pub const DEFAULT_USER_STACK_BOTTOM: VirtAddr = MMArch::USER_STACK_START;
    /// 默认的用户栈大小为8MB
    pub const DEFAULT_USER_STACK_SIZE: usize = 8 * 1024 * 1024;
    /// 用户栈的保护页数量
    pub const GUARD_PAGES_NUM: usize = 4;

    /// 创建一个用户栈
    pub fn new(
        vm: &mut InnerAddressSpace,
        stack_bottom: Option<VirtAddr>,
        stack_size: usize,
    ) -> Result<Self, SystemError> {
        let stack_bottom = stack_bottom.unwrap_or(Self::DEFAULT_USER_STACK_BOTTOM);
        assert!(stack_bottom.check_aligned(MMArch::PAGE_SIZE));

        // Layout
        // -------------- high->sp
        // | stack pages|
        // |------------|
        // | not mapped |
        // -------------- low

        let prot_flags = ProtFlags::PROT_READ | ProtFlags::PROT_WRITE | ProtFlags::PROT_EXEC;
        let map_flags = MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS | MapFlags::MAP_GROWSDOWN;

        let stack_size = page_align_up(stack_size);

        // log::info!(
        //     "UserStack stack_range: {:#x} - {:#x}",
        //     stack_bottom.data() - stack_size,
        //     stack_bottom.data()
        // );

        vm.map_anonymous(
            stack_bottom - stack_size,
            stack_size,
            prot_flags,
            map_flags,
            false,
            false,
        )?;

        let max_limit = core::cmp::max(Self::DEFAULT_USER_STACK_SIZE, stack_size);

        let user_stack = UserStack {
            stack_bottom,
            mapped_size: stack_size,
            current_sp: stack_bottom,
            max_limit,
        };

        return Ok(user_stack);
    }

    /// 获取栈顶地址
    ///
    /// 请注意，如果用户栈的栈顶地址发生变化，这个值可能不会实时更新！
    pub fn sp(&self) -> VirtAddr {
        return self.current_sp;
    }

    pub unsafe fn set_sp(&mut self, sp: VirtAddr) {
        self.current_sp = sp;
    }

    /// 仅仅克隆用户栈的信息，不会克隆用户栈的内容/映射
    pub unsafe fn clone_info_only(&self) -> Self {
        return Self {
            stack_bottom: self.stack_bottom,
            mapped_size: self.mapped_size,
            current_sp: self.current_sp,
            max_limit: self.max_limit,
        };
    }

    /// 获取当前用户栈的大小（不包括保护页）
    pub fn stack_size(&self) -> usize {
        return self.mapped_size;
    }

    /// 设置当前用户栈的最大大小
    pub fn set_max_limit(&mut self, max_limit: usize) {
        self.max_limit = max_limit;
    }

    /// 获取当前用户栈的最大大小限制
    pub fn max_limit(&self) -> usize {
        self.max_limit
    }
}

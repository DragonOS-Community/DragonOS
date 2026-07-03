use super::*;

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
    /// Globally unique address space ID, used to identify different address spaces
    /// This ID remains unchanged throughout the address space's lifecycle and is never reused
    id: u64,
    /// Page table physical address (unchanged after creation, lock-free access)
    /// Used for fast page table switching in scheduler context without acquiring the RwSem lock
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
    /// Uses RwSem instead of RwLock because address space operations may require I/O (e.g., file reads on page faults)
    inner: RwSem<InnerAddressSpace>,
    /// Wait for pending mmap reservations to be committed or cancelled.
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

    /// Get the globally unique ID of this address space
    #[inline(always)]
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Get the page table physical address (lock-free access)
    /// Used for fast page table switching in scheduler context
    #[inline(always)]
    pub fn table_paddr(&self) -> PhysAddr {
        self.table_paddr
    }

    /// Get the current process's address space Arc pointer from the PCB
    pub fn current() -> Result<Arc<AddressSpace>, SystemError> {
        let vm = ProcessManager::current_pcb()
            .basic()
            .user_vm()
            .expect("Current process has no address space");

        return Ok(vm);
    }

    /// Check whether this address space belongs to the current process
    pub fn is_current(self: &Arc<Self>) -> bool {
        let current = Self::current();
        if let Ok(current) = current {
            return Arc::ptr_eq(&current, self);
        }
        return false;
    }

    /// Set this address space's page table as the current page table (lock-free)
    ///
    /// This method is used for fast page table switching in scheduler context, without acquiring the RwSem lock.
    /// Safety is guaranteed by the caller: only use during context switches.
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

    pub(super) fn round_mmap_hint(
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
                    Err(failure) => {
                        close_notifications.extend(failure.notifications);
                        map_fail!(failure.err);
                    }
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
            match guard.munmap_collect(start_page, page_count) {
                Ok(notifications) => {
                    drop(guard);
                    InnerAddressSpace::notify_close_notifications(notifications);
                    return Ok(());
                }
                Err(failure) => {
                    drop(guard);
                    InnerAddressSpace::notify_close_notifications(failure.notifications);
                    return Err(failure.err);
                }
            }
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
            match guard.mprotect_collect(start_page, page_count, prot_flags) {
                Ok(()) => return Ok(()),
                Err(failure) => {
                    drop(guard);
                    InnerAddressSpace::notify_close_notifications(failure.notifications);
                    return Err(failure.err);
                }
            }
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
            match guard.madvise_collect(start_page, page_count, behavior) {
                Ok(()) => return Ok(()),
                Err(failure) => {
                    drop(guard);
                    InnerAddressSpace::notify_close_notifications(failure.notifications);
                    return Err(failure.err);
                }
            }
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

use core::{
    alloc::Layout,
    intrinsics::{likely, unlikely},
    panic,
};

use alloc::sync::Arc;
use x86::{bits64::rflags::RFlags, controlregs::Cr4};

use crate::{
    arch::{
        interrupt::{trap::X86PfErrorCode, TrapFrame},
        mm::{MemoryManagementArch, X86_64MMArch},
        CurrentIrqArch, MMArch,
    },
    exception::InterruptArch,
    kerror,
    mm::{
        page::{page_manager_lock_irqsave, PageFlags},
        ucontext::{AddressSpace, LockedVMA},
        VirtAddr, VmFaultReason, VmFlags,
    },
    process::{ProcessManager, ProcessState},
};

use super::LockedFrameAllocator;

pub type PageMapper =
    crate::mm::page::PageMapper<crate::arch::x86_64::mm::X86_64MMArch, LockedFrameAllocator>;

bitflags! {
    pub struct FaultFlags: u64{
    const FAULT_FLAG_WRITE = 1 << 0;
    const FAULT_FLAG_MKWRITE = 1 << 1;
    const FAULT_FLAG_ALLOW_RETRY = 1 << 2;
    const FAULT_FLAG_RETRY_NOWAIT = 1 << 3;
    const FAULT_FLAG_KILLABLE = 1 << 4;
    const FAULT_FLAG_TRIED = 1 << 5;
    const FAULT_FLAG_USER = 1 << 6;
    const FAULT_FLAG_REMOTE = 1 << 7;
    const FAULT_FLAG_INSTRUCTION = 1 << 8;
    const FAULT_FLAG_INTERRUPTIBLE =1 << 9;
    const FAULT_FLAG_UNSHARE = 1 << 10;
    const FAULT_FLAG_ORIG_PTE_VALID = 1 << 11;
    const FAULT_FLAG_VMA_LOCK = 1 << 12;
    }
}

pub struct PageFaultHandler;

impl PageFaultHandler {
    pub fn show_fault_oops(
        regs: &'static TrapFrame,
        error_code: X86PfErrorCode,
        address: VirtAddr,
    ) {
        let mapper =
            unsafe { PageMapper::current(crate::mm::PageTableKind::User, LockedFrameAllocator) };
        if let Some(entry) = mapper.get_entry(address, 0) {
            if entry.present() {
                if !entry.flags().has_execute() {
                    kerror!("kernel tried to execute NX-protected page - exploit attempt?");
                } else if mapper.table().phys().data() & MMArch::ENTRY_FLAG_USER != 0
                    && unsafe { x86::controlregs::cr4().contains(Cr4::CR4_ENABLE_SMEP) }
                {
                    kerror!("unable to execute userspace code (SMEP?)");
                }
            }
        }
        if address.data() < X86_64MMArch::PAGE_SIZE && !regs.is_from_user() {
            kerror!(
                "BUG: kernel NULL pointer dereference, address: {:#x}",
                address.data()
            );
        } else {
            kerror!(
                "BUG: unable to handle page fault for address: {:#x}",
                address.data()
            );
        }

        kerror!(
            "#PF: {} {} in {} mode\n",
            if error_code.contains(X86PfErrorCode::X86_PF_USER) {
                "user"
            } else {
                "supervisor"
            },
            if error_code.contains(X86PfErrorCode::X86_PF_INSTR) {
                "instruction fetch"
            } else if error_code.contains(X86PfErrorCode::X86_PF_WRITE) {
                "write access"
            } else {
                "read access"
            },
            if regs.is_from_user() {
                "user"
            } else {
                "kernel"
            }
        );
        kerror!(
            "#PF: error_code({:#04x}) - {}\n",
            error_code,
            if !error_code.contains(X86PfErrorCode::X86_PF_PROT) {
                "not-present page"
            } else if error_code.contains(X86PfErrorCode::X86_PF_RSVD) {
                "reserved bit violation"
            } else if error_code.contains(X86PfErrorCode::X86_PF_PK) {
                "protection keys violation"
            } else {
                "permissions violation"
            }
        );
    }

    pub fn page_fault_oops(
        regs: &'static TrapFrame,
        error_code: X86PfErrorCode,
        address: VirtAddr,
    ) {
        if regs.is_from_user() {
            Self::show_fault_oops(regs, error_code, address);
        }
        panic!()
    }

    pub fn do_kern_addr_fault(
        _regs: &'static TrapFrame,
        _error_code: X86PfErrorCode,
        _address: VirtAddr,
    ) {
    }

    pub unsafe fn do_user_addr_fault(
        regs: &'static TrapFrame,
        error_code: X86PfErrorCode,
        address: VirtAddr,
    ) {
        let rflags = RFlags::from_bits_truncate(regs.rflags);
        let mut flags: FaultFlags = FaultFlags::FAULT_FLAG_ALLOW_RETRY
            | FaultFlags::FAULT_FLAG_KILLABLE
            | FaultFlags::FAULT_FLAG_INTERRUPTIBLE;

        if error_code & (X86PfErrorCode::X86_PF_USER | X86PfErrorCode::X86_PF_INSTR)
            == X86PfErrorCode::X86_PF_INSTR
        {
            Self::page_fault_oops(regs, error_code, address);
        }

        let feature = x86::cpuid::CpuId::new()
            .get_extended_feature_info()
            .unwrap();
        if unlikely(
            feature.has_smap()
                && !error_code.contains(X86PfErrorCode::X86_PF_USER)
                && rflags.contains(RFlags::FLAGS_AC),
        ) {
            Self::page_fault_oops(regs, error_code, address);
        }

        if unlikely(error_code.contains(X86PfErrorCode::X86_PF_RSVD)) {
            panic!("Bad pagetable");
        }

        if regs.is_from_user() {
            unsafe { CurrentIrqArch::interrupt_enable() };
            flags |= FaultFlags::FAULT_FLAG_USER;
        } else if rflags.contains(RFlags::FLAGS_IF) {
            unsafe { CurrentIrqArch::interrupt_enable() };
        }

        if error_code.contains(X86PfErrorCode::X86_PF_SHSTK) {
            flags |= FaultFlags::FAULT_FLAG_WRITE;
        }
        if error_code.contains(X86PfErrorCode::X86_PF_WRITE) {
            flags |= FaultFlags::FAULT_FLAG_WRITE;
        }
        if error_code.contains(X86PfErrorCode::X86_PF_INSTR) {
            flags |= FaultFlags::FAULT_FLAG_INSTRUCTION;
        }

        let current_address_space: Arc<AddressSpace> = AddressSpace::current().unwrap();
        let mut space_guard = current_address_space.write();
        let mut fault;
        loop {
            // let vma = space_guard.mappings.find_nearest(address);
            let vma = space_guard.mappings.contains(address);
            if vma.is_none() {
                panic!("no mapped vma");
            }
            let address = VirtAddr::new(address.data() & MMArch::PAGE_MASK);

            let vma = vma.unwrap();
            // let guard = vma.lock();

            // if !guard.region().contains(address) && guard.vm_flags().contains(VmFlags::VM_GROWSDOWN)
            // {
            //     space_guard
            //         .extend_stack(guard.region().start() - address)
            //         .expect("User stack extend failed");
            // }
            // drop(guard);

            if unlikely(vma.access_error(error_code)) {
                panic!("vma access error");
            }
            let mapper = &mut space_guard.user_mapper.utable;

            fault = Self::handle_mm_fault(vma.clone(), mapper, address, flags, regs);

            if !fault.contains(VmFaultReason::VM_FAULT_COMPLETED) {
                return;
            }

            if unlikely(fault.contains(VmFaultReason::VM_FAULT_RETRY)) {
                flags |= FaultFlags::FAULT_FLAG_TRIED;
            } else {
                break;
            }
        }

        let vm_fault_error = VmFaultReason::VM_FAULT_OOM
            | VmFaultReason::VM_FAULT_SIGBUS
            | VmFaultReason::VM_FAULT_SIGSEGV
            | VmFaultReason::VM_FAULT_HWPOISON
            | VmFaultReason::VM_FAULT_HWPOISON_LARGE
            | VmFaultReason::VM_FAULT_FALLBACK;

        if likely(!fault.contains(vm_fault_error)) {
            return;
        }

        if fault.contains(VmFaultReason::VM_FAULT_OOM) {
            panic!("page fault out of memory");
        }
    }

    pub unsafe fn handle_mm_fault(
        vma: Arc<LockedVMA>,
        mapper: &mut PageMapper,
        address: VirtAddr,
        flags: FaultFlags,
        _regs: &'static TrapFrame,
    ) -> VmFaultReason {
        let current_pcb = ProcessManager::current_pcb();
        let mut guard = current_pcb.sched_info().inner_lock_write_irqsave();
        guard.set_state(ProcessState::Runnable);

        if !vma.access_permitted(
            flags.contains(FaultFlags::FAULT_FLAG_WRITE),
            flags.contains(FaultFlags::FAULT_FLAG_INSTRUCTION),
            flags.contains(FaultFlags::FAULT_FLAG_REMOTE),
        ) {
            return VmFaultReason::VM_FAULT_SIGSEGV;
        }

        let guard = vma.lock();
        let vm_flags = *guard.vm_flags();
        drop(guard);
        if unlikely(vm_flags.contains(VmFlags::VM_HUGETLB)) {
            //TODO: 添加handle_hugetlb_fault
        } else {
            Self::handle_normal_fault(vma.clone(), mapper, address, flags);
        }

        VmFaultReason::VM_FAULT_COMPLETED
    }

    pub unsafe fn handle_normal_fault(
        vma: Arc<LockedVMA>,
        mapper: &mut PageMapper,
        address: VirtAddr,
        flags: FaultFlags,
    ) -> VmFaultReason {
        if mapper.get_entry(address, 3).is_none() {
            mapper
                .allocate_table(address, 2)
                .expect("failed to allocate PUD table");
        }
        let page_flags = vma.lock().flags();

        for level in 2..=3 {
            let level = MMArch::PAGE_LEVELS - level;
            if mapper.get_entry(address, level).is_none() {
                if vma.is_hugepage() {
                    if vma.is_anonymous() {
                        mapper.map_huge_page(address, page_flags);
                    }
                } else if mapper.allocate_table(address, level - 1).is_none() {
                    return VmFaultReason::VM_FAULT_OOM;
                }
            }
        }

        Self::handle_pte_fault(vma, mapper, address, flags)
    }

    pub unsafe fn handle_pte_fault(
        vma: Arc<LockedVMA>,
        mapper: &mut PageMapper,
        address: VirtAddr,
        flags: FaultFlags,
    ) -> VmFaultReason {
        if let Some(mut entry) = mapper.get_entry(address, 0) {
            if !entry.present() {
                return Self::do_swap_page(vma, mapper, address, flags);
            }
            if entry.protnone() && vma.is_accessible() {
                return Self::do_numa_page(vma, mapper, address, flags);
            }
            if flags.intersects(FaultFlags::FAULT_FLAG_WRITE | FaultFlags::FAULT_FLAG_UNSHARE) {
                if !entry.write() {
                    return Self::do_wp_page(vma, mapper, address);
                } else {
                    entry.set_flags(PageFlags::from_data(MMArch::ENTRY_FLAG_DIRTY));
                }
            }
            let pte_table = mapper.get_table(address, 0).unwrap();
            let i = pte_table.index_of(address).unwrap();
            entry.set_flags(entry.flags().set_access(true));
            pte_table.set_entry(i, entry);
        } else if vma.is_anonymous() {
            return Self::do_anonymous_page(vma, mapper, address);
        } else {
            return Self::do_fault(vma, mapper, address, flags);
        }

        VmFaultReason::VM_FAULT_COMPLETED
    }

    pub unsafe fn do_anonymous_page(
        vma: Arc<LockedVMA>,
        mapper: &mut PageMapper,
        address: VirtAddr,
    ) -> VmFaultReason {
        if let Some(flush) = mapper.map(address, vma.lock().flags()) {
            flush.flush();
            crate::debug::klog::mm::mm_debug_log(
                klog_types::AllocatorLogType::LazyAlloc(klog_types::AllocLogItem::new(
                    Layout::from_size_align(MMArch::PAGE_SIZE, MMArch::PAGE_SIZE).unwrap(),
                    Some(address.data()),
                    Some(mapper.translate(address).unwrap().0.data()),
                )),
                klog_types::LogSource::Buddy,
            );
            let paddr = mapper.translate(address).unwrap().0;
            let mut anon_vma_guard = page_manager_lock_irqsave();
            let page = anon_vma_guard.get_mut(&paddr);
            page.insert_vma(vma.clone());
            VmFaultReason::VM_FAULT_COMPLETED
        } else {
            VmFaultReason::VM_FAULT_OOM
        }
    }

    pub unsafe fn do_fault(
        _vma: Arc<LockedVMA>,
        _mapper: &mut PageMapper,
        _address: VirtAddr,
        _flags: FaultFlags,
    ) -> VmFaultReason {
        panic!("do_fault has not yet been implemented");
    }

    pub unsafe fn do_swap_page(
        _vma: Arc<LockedVMA>,
        _mapper: &mut PageMapper,
        _address: VirtAddr,
        _flags: FaultFlags,
    ) -> VmFaultReason {
        panic!("do_swap_page has not yet been implemented");
    }

    pub unsafe fn do_numa_page(
        _vma: Arc<LockedVMA>,
        _mapper: &mut PageMapper,
        _address: VirtAddr,
        _flags: FaultFlags,
    ) -> VmFaultReason {
        panic!("do_numa_page has not yet been implemented");
    }

    pub unsafe fn do_wp_page(
        vma: Arc<LockedVMA>,
        mapper: &mut PageMapper,
        address: VirtAddr,
    ) -> VmFaultReason {
        let old_paddr = mapper.translate(address).unwrap().0;
        let mut page_manager = page_manager_lock_irqsave();
        let map_count = page_manager.get_mut(&old_paddr).map_count;
        drop(page_manager);

        let mut entry = mapper.get_entry(address, 0).unwrap();
        let new_flags = entry.flags().set_write(true);

        if map_count == 1 {
            let table = mapper.get_table(address, 0).unwrap();
            let i = table.index_of(address).unwrap();
            entry.set_flags(new_flags);
            table.set_entry(i, entry);
            VmFaultReason::VM_FAULT_COMPLETED
        } else if let Some(flush) = mapper.map(address, new_flags) {
            let mut page_manager = page_manager_lock_irqsave();
            let old_page = page_manager.get_mut(&old_paddr);
            old_page.remove_vma(&vma);
            drop(page_manager);

            flush.flush();
            let paddr = mapper.translate(address).unwrap().0;
            let mut anon_vma_guard = page_manager_lock_irqsave();
            let page = anon_vma_guard.get_mut(&paddr);
            page.insert_vma(vma.clone());

            (MMArch::phys_2_virt(paddr).unwrap().data() as *mut u8).copy_from_nonoverlapping(
                MMArch::phys_2_virt(old_paddr).unwrap().data() as *mut u8,
                MMArch::PAGE_SIZE,
            );

            VmFaultReason::VM_FAULT_COMPLETED
        } else {
            VmFaultReason::VM_FAULT_OOM
        }
    }
}

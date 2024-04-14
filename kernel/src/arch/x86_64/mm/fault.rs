use core::{
    intrinsics::{likely, unlikely},
    panic,
};

use alloc::sync::Arc;
use x86::{bits64::rflags::RFlags, controlregs::Cr4};

use crate::{
    arch::{
        interrupt::{trap::X86PfErrorCode, TrapFrame},
        mm::{MemoryManagementArch, X86_64MMArch},
        CurrentIrqArch, MMArch, ProtectionKey,
    },
    exception::InterruptArch,
    kerror,
    mm::{
        fault::{FaultFlags, PageFault, PageFaultHandler, PageFaultMessage},
        ucontext::{AddressSpace, LockedVMA},
        ProtectionKeyTrait, VirtAddr, VmFaultReason, VmFlags,
    },
};

use super::LockedFrameAllocator;

pub type PageMapper =
    crate::mm::page::PageMapper<crate::arch::x86_64::mm::X86_64MMArch, LockedFrameAllocator>;

pub struct X86_64PageFault;

impl PageFault for X86_64PageFault {
    fn vma_access_permitted(
        vma: Arc<LockedVMA>,
        write: bool,
        execute: bool,
        foreign: bool,
    ) -> bool {
        if execute {
            return true;
        }
        if foreign | vma.is_foreign() {
            return true;
        }
        super::pkey::pkru_allows_pkey(ProtectionKey::vma_pkey(vma), write)
    }
}

impl X86_64PageFault {
    pub fn vma_access_error(vma: Arc<LockedVMA>, error_code: X86PfErrorCode) -> bool {
        let vm_flags = *vma.lock().vm_flags();
        let foreign = false;
        if error_code.contains(X86PfErrorCode::X86_PF_PK) {
            return true;
        }

        if unlikely(error_code.contains(X86PfErrorCode::X86_PF_SGX)) {
            return true;
        }

        if !X86_64PageFault::vma_access_permitted(
            vma.clone(),
            error_code.contains(X86PfErrorCode::X86_PF_WRITE),
            error_code.contains(X86PfErrorCode::X86_PF_INSTR),
            foreign,
        ) {
            return true;
        }

        if error_code.contains(X86PfErrorCode::X86_PF_WRITE) {
            if unlikely(!vm_flags.contains(VmFlags::VM_WRITE)) {
                return true;
            }
            return false;
        }

        if unlikely(error_code.contains(X86PfErrorCode::X86_PF_PROT)) {
            return true;
        }

        if unlikely(!vma.is_accessible()) {
            return true;
        }
        false
    }

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

    /// 内核态缺页异常处理
    /// ## 参数
    ///
    /// - `regs`: 中断栈帧
    /// - `error_code`: 错误标志
    /// - `address`: 发生缺页异常的虚拟地址
    pub fn do_kern_addr_fault(
        _regs: &'static TrapFrame,
        error_code: X86PfErrorCode,
        address: VirtAddr,
    ) {
        panic!(
            "do_kern_addr_fault has not yet been implemented, 
        fault address: {:#x}, 
        error_code: {:#b}, 
        pid: {}\n",
            address.data(),
            error_code,
            crate::process::ProcessManager::current_pid().data()
        );
        //TODO https://code.dragonos.org.cn/xref/linux-6.6.21/arch/x86/mm/fault.c#do_kern_addr_fault
    }

    /// 用户态缺页异常处理
    /// ## 参数
    ///
    /// - `regs`: 中断栈帧
    /// - `error_code`: 错误标志
    /// - `address`: 发生缺页异常的虚拟地址
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

            if unlikely(Self::vma_access_error(vma.clone(), error_code)) {
                panic!("vma access error");
            }
            let mapper = &mut space_guard.user_mapper.utable;

            fault = PageFaultHandler::handle_mm_fault(
                PageFaultMessage {
                    vma: vma.clone(),
                    address,
                    flags,
                },
                mapper,
            );

            if fault.contains(VmFaultReason::VM_FAULT_COMPLETED) {
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
            panic!("{:?}", fault)
        }
    }
}
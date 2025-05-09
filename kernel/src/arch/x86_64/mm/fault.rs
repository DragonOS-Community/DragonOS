use core::{
    intrinsics::{likely, unlikely},
    panic,
};

use alloc::sync::Arc;
use log::error;
use x86::{bits64::rflags::RFlags, controlregs::Cr4};

use crate::{
    arch::{
        interrupt::{trap::X86PfErrorCode, TrapFrame},
        ipc::signal::{SigCode, Signal},
        mm::{MemoryManagementArch, X86_64MMArch},
        CurrentIrqArch, MMArch,
    },
    exception::InterruptArch,
    ipc::signal_types::{SigInfo, SigType},
    mm::{
        fault::{FaultFlags, PageFaultHandler, PageFaultMessage},
        ucontext::{AddressSpace, LockedVMA},
        VirtAddr, VmFaultReason, VmFlags,
    },
    process::ProcessManager,
};

use super::LockedFrameAllocator;

pub type PageMapper =
    crate::mm::page::PageMapper<crate::arch::x86_64::mm::X86_64MMArch, LockedFrameAllocator>;

impl X86_64MMArch {
    pub fn vma_access_error(vma: Arc<LockedVMA>, error_code: X86PfErrorCode) -> bool {
        let vm_flags = *vma.lock_irqsave().vm_flags();
        let foreign = false;
        if error_code.contains(X86PfErrorCode::X86_PF_PK) {
            return true;
        }

        if unlikely(error_code.contains(X86PfErrorCode::X86_PF_SGX)) {
            return true;
        }

        if !Self::vma_access_permitted(
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
                    error!("kernel tried to execute NX-protected page - exploit attempt?");
                } else if mapper.table().phys().data() & MMArch::ENTRY_FLAG_USER != 0
                    && unsafe { x86::controlregs::cr4().contains(Cr4::CR4_ENABLE_SMEP) }
                {
                    error!("unable to execute userspace code (SMEP?)");
                }
            }
        }
        if address.data() < X86_64MMArch::PAGE_SIZE && !regs.is_from_user() {
            error!(
                "BUG: kernel NULL pointer dereference, address: {:#x}",
                address.data()
            );
        } else {
            error!(
                "BUG: unable to handle page fault for address: {:#x}",
                address.data()
            );
        }

        error!(
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
        error!(
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
            // TODO https://code.dragonos.org.cn/xref/linux-6.6.21/arch/x86/mm/fault.c#pgtable_bad
            panic!(
                "Reserved bits are never expected to be set, error_code: {:#b}, address: {:#x}",
                error_code,
                address.data()
            );
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
        let mut space_guard = current_address_space.write_irqsave();
        let mut fault;
        loop {
            let vma = space_guard.mappings.find_nearest(address);
            // let vma = space_guard.mappings.contains(address);

            let vma = match vma {
                Some(vma) => vma,
                None => {
                    log::error!(
                        "can not find nearest vma, error_code: {:#b}, address: {:#x}",
                        error_code,
                        address.data(),
                    );
                    let pid = ProcessManager::current_pid();
                    let mut info =
                        SigInfo::new(Signal::SIGSEGV, 0, SigCode::User, SigType::Kill(pid));
                    Signal::SIGSEGV
                        .send_signal_info(Some(&mut info), pid)
                        .expect("failed to send SIGSEGV to process");
                    return;
                }
            };
            let guard = vma.lock_irqsave();
            let region = *guard.region();
            let vm_flags = *guard.vm_flags();
            drop(guard);

            if !region.contains(address) {
                if vm_flags.contains(VmFlags::VM_GROWSDOWN) {
                    space_guard
                        .extend_stack(region.start() - address)
                        .unwrap_or_else(|_| {
                            panic!(
                                "user stack extend failed, error_code: {:#b}, address: {:#x}",
                                error_code,
                                address.data(),
                            )
                        });
                } else {
                    log::error!(
                        "No mapped vma, error_code: {:#b}, address: {:#x}, flags: {:?}",
                        error_code,
                        address.data(),
                        flags
                    );
                    log::error!("fault rip: {:#x}", regs.rip);

                    let pid = ProcessManager::current_pid();
                    let mut info =
                        SigInfo::new(Signal::SIGSEGV, 0, SigCode::User, SigType::Kill(pid));
                    Signal::SIGSEGV
                        .send_signal_info(Some(&mut info), pid)
                        .expect("failed to send SIGSEGV to process");
                    return;
                }
            }

            if unlikely(Self::vma_access_error(vma.clone(), error_code)) {
                panic!(
                    "vma access error, error_code: {:#b}, address: {:#x}",
                    error_code,
                    address.data(),
                );
            }
            let mapper = &mut space_guard.user_mapper.utable;
            let message = PageFaultMessage::new(vma.clone(), address, flags, mapper);

            fault = PageFaultHandler::handle_mm_fault(message);

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
            panic!("fault error: {:?}", fault)
        }
    }
}

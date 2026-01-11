use alloc::sync::Arc;
use core::{intrinsics::unlikely, panic};
use log::error;
use x86::{bits64::rflags::RFlags, controlregs::Cr4};

use crate::{
    arch::{
        interrupt::{trap::X86PfErrorCode, TrapFrame},
        ipc::signal::Signal,
        mm::{MemoryManagementArch, X86_64MMArch},
        CurrentIrqArch, MMArch,
    },
    exception::{extable::ExceptionTableManager, InterruptArch},
    ipc::signal_types::{SigCode, SigInfo, SigType},
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
        let vm_flags = *vma.read().vm_flags();
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

    pub fn show_fault_oops(regs: &TrapFrame, error_code: X86PfErrorCode, address: VirtAddr) {
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

    pub fn page_fault_oops(regs: &TrapFrame, error_code: X86PfErrorCode, address: VirtAddr) {
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
        regs: &'static mut TrapFrame,
        error_code: X86PfErrorCode,
        address: VirtAddr,
    ) {
        // 尝试异常表修复
        if Self::try_fixup_exception(regs, error_code, address) {
            // 成功修复,直接返回
            return;
        }

        unsafe { crate::debug::traceback::lookup_kallsyms(regs.rip, 0xff) };
        let pcb = crate::process::ProcessManager::current_pcb();
        let kstack_guard_addr = pcb.kernel_stack().guard_page_address();
        if let Some(guard_page) = kstack_guard_addr {
            let guard_page_size = pcb.kernel_stack().guard_page_size().unwrap();
            if address.data() >= guard_page.data()
                && address.data() < guard_page.data() + guard_page_size
            {
                // 发生在内核栈保护页上
                error!(
                    "kernel stack guard page fault at {:#x}, guard page range: {:#x} - {:#x}",
                    address.data(),
                    guard_page.data(),
                    guard_page.data() + guard_page_size
                );
            }
        }
        panic!(
            "do_kern_addr_fault has not yet been implemented, 
        fault address: {:#x},
        rip: {:#x},
        error_code: {:#b}, 
        pid: {}\n",
            address.data(),
            regs.rip,
            error_code,
            pcb.raw_pid().data()
        );
        //TODO https://code.dragonos.org.cn/xref/linux-6.6.21/arch/x86/mm/fault.c#do_kern_addr_fault
    }

    /// 尝试使用异常表修复页错误
    ///
    /// ## 返回值
    /// - `true`: 成功修复,可以继续执行
    /// - `false`: 无法修复,是真正的内核错误
    #[inline(never)]
    fn try_fixup_exception(
        regs: &mut TrapFrame,
        _error_code: X86PfErrorCode,
        address: VirtAddr,
    ) -> bool {
        // 只处理用户空间地址的访问错误
        if !address.check_user() {
            return false;
        }

        // 在异常表中查找修复代码
        if let Some(fixup_addr) = ExceptionTableManager::search_exception_table(regs.rip as usize) {
            // log::debug!(
            //     "Page fault at {:#x} accessing user address {:#x}, fixed up to {:#x}",
            //     regs.rip,
            //     address.data(),
            //     fixup_addr
            // );

            // 修改trap frame的RIP到修复代码
            regs.rip = fixup_addr as u64;

            return true;
        }

        false
    }

    /// 用户态缺页异常处理
    /// ## 参数
    ///
    /// - `regs`: 中断栈帧
    /// - `error_code`: 错误标志
    /// - `address`: 发生缺页异常的虚拟地址
    pub unsafe fn do_user_addr_fault(
        regs: &'static mut TrapFrame,
        error_code: X86PfErrorCode,
        address: VirtAddr,
    ) {
        // log::debug!("fault at {:?}:{:?}",
        // address,
        // error_code,
        // );
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

        let send_segv = || {
            let pid = ProcessManager::current_pid();
            let uid = ProcessManager::current_pcb().cred().uid.data() as u32;
            let mut info = SigInfo::new(
                Signal::SIGSEGV,
                0,
                SigCode::User,
                SigType::Kill { pid, uid },
            );
            Signal::SIGSEGV
                .send_signal_info(Some(&mut info), pid)
                .expect("failed to send SIGSEGV to process");
        };

        // 辅助函数：处理内核访问用户地址失败的情况
        let handle_kernel_access_failed = |r: &mut TrapFrame| {
            // 如果是内核代码访问用户地址，尝试异常表修复
            if !r.is_from_user() {
                if Self::try_fixup_exception(r, error_code, address) {
                    return true; // 成功修复
                }
                // 如果异常表中没有，说明是bug
                error!(
                    "Kernel code at {:#x} illegally accessed user address {:#x} \
                     without exception table entry",
                    r.rip,
                    address.data()
                );
                panic!("Illegal user space access from kernel");
            }
            false // 不是内核访问，继续正常流程
        };

        let current_address_space: Arc<AddressSpace> = AddressSpace::current().unwrap();
        let mut space_guard = current_address_space.write();
        let mut fault;
        loop {
            let vma = space_guard.mappings.find_nearest(address);
            let vma = match vma {
                Some(vma) => vma,
                None => {
                    log::error!(
                        "pid:{}, can not find nearest vma, \n\terror_code: {:?}, address: {:#x}, rip: {:#x}",
                        ProcessManager::current_pid().data(),
                        error_code,
                        address.data(),
                        regs.rip,
                    );

                    // VMA不存在，检查是否需要异常表修复
                    if handle_kernel_access_failed(regs) {
                        return; // 已通过异常表修复
                    }

                    send_segv();
                    return;
                }
            };
            let guard = vma.read();
            let region = *guard.region();
            let vm_flags = *guard.vm_flags();
            drop(guard);

            if !region.contains(address) {
                if vm_flags.contains(VmFlags::VM_GROWSDOWN) {
                    let extension_size = region.start() - address;

                    // 首先检查地址是否在栈的合理扩展范围内
                    // 如果地址距离栈底太远（超过最大栈限制），则这不是一个栈扩展请求，
                    // 而是一个无关的无效内存访问（例如空指针解引用）
                    let max_stack_limit = space_guard
                        .user_stack
                        .as_ref()
                        .map(|s| s.max_limit())
                        .unwrap_or(0);

                    if extension_size > max_stack_limit {
                        // 地址距离栈太远，这不是栈扩展请求，而是普通的无效内存访问
                        // 检查是否需要异常表修复
                        if handle_kernel_access_failed(regs) {
                            return; // 已通过异常表修复
                        }

                        send_segv();
                        return;
                    }

                    if !space_guard.can_extend_stack(extension_size) {
                        // 栈扩展超过限制
                        log::warn!(
                            "pid {} user stack limit exceeded, error_code: {:?}, address: {:#x}",
                            ProcessManager::current_pid().data(),
                            error_code,
                            address.data(),
                        );

                        // 栈溢出，检查是否需要异常表修复
                        if handle_kernel_access_failed(regs) {
                            return; // 已通过异常表修复
                        }

                        send_segv();
                        return;
                    }
                    space_guard
                        .extend_stack(extension_size)
                        .unwrap_or_else(|_| {
                            panic!(
                                "user stack extend failed, error_code: {:?}, address: {:#x}",
                                error_code,
                                address.data(),
                            )
                        });
                } else {
                    log::error!(
                        "pid: {} No mapped vma, error_code: {:?},rip:{:#x}, address: {:#x}, flags: {:?}",
                        ProcessManager::current_pid().data(),
                        error_code,
                        regs.rip,
                        address.data(),
                        flags
                    );
                    log::error!("fault rip: {:#x}", regs.rip);

                    // 地址不在VMA范围内，检查是否需要异常表修复
                    if handle_kernel_access_failed(regs) {
                        return; // 已通过异常表修复
                    }

                    send_segv();
                    return;
                }
            }

            if unlikely(Self::vma_access_error(vma.clone(), error_code)) {
                // VMA权限错误，检查是否需要异常表修复
                if handle_kernel_access_failed(regs) {
                    return; // 已通过异常表修复
                }

                // log::error!(
                //     "vma access error, error_code: {:?}, address: {:#x}",
                //     error_code,
                //     address.data(),
                // );

                send_segv();
                return;
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

        if fault.intersects(vm_fault_error) {
            // 内核态访问用户地址（如 copy_from_user）应走异常表修复，返回 -EFAULT，而不是发送信号/崩溃
            if !regs.is_from_user() {
                if Self::try_fixup_exception(regs, error_code, address) {
                    return;
                }
                panic!(
                    "kernel access to user addr failed without fixup, fault: {:?}, addr: {:#x}, rip: {:#x}",
                    fault,
                    address.data(),
                    regs.rip
                );
            }

            // 用户态 fault：发送对应信号
            let sig = if fault.contains(VmFaultReason::VM_FAULT_SIGSEGV) {
                Signal::SIGSEGV
            } else {
                // 包括 SIGBUS / OOM / HWPOISON 等：目前统一 SIGBUS（后续可按 Linux 进一步细分）
                Signal::SIGBUS
            };

            let mut info = SigInfo::new(
                sig,
                0,
                SigCode::User,
                SigType::Kill {
                    pid: ProcessManager::current_pid(),
                    uid: ProcessManager::current_pcb().cred().uid.data() as u32,
                },
            );
            let _ = sig.send_signal_info(Some(&mut info), ProcessManager::current_pid());
            return;
        }

        panic!("fault error: {:?}", fault)
    }
}

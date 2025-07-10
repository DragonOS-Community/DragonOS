use core::{
    arch::asm,
    intrinsics::unlikely,
    mem::ManuallyDrop,
    sync::atomic::{compiler_fence, Ordering},
};

use alloc::sync::{Arc, Weak};

use kdepends::memoffset::offset_of;
use log::{error, warn};
use system_error::SystemError;
use x86::{controlregs::Cr4, segmentation::SegmentSelector};

use crate::{
    arch::process::table::TSSManager,
    exception::InterruptArch,
    libs::spinlock::SpinLockGuard,
    mm::VirtAddr,
    process::{
        fork::{CloneFlags, KernelCloneArgs},
        KernelStack, ProcessControlBlock, ProcessFlags, ProcessManager, PROCESS_SWITCH_RESULT,
    },
    syscall::Syscall,
};

use self::{
    kthread::kernel_thread_bootstrap_stage1,
    syscall::ARCH_SET_FS,
    table::{switch_fs_and_gs, KERNEL_DS, USER_DS},
};

use super::{fpu::FpState, interrupt::TrapFrame, syscall::X86_64GSData, CurrentIrqArch};

pub mod idle;
pub mod kthread;
pub mod syscall;
pub mod table;

extern "C" {
    /// 从中断返回
    fn ret_from_intr();
}

#[allow(dead_code)]
#[repr(align(32768))]
union InitProcUnion {
    /// 用于存放idle进程的内核栈
    idle_stack: [u8; 32768],
}

#[link_section = ".data.init_proc_union"]
#[no_mangle]
static BSP_IDLE_STACK_SPACE: InitProcUnion = InitProcUnion {
    idle_stack: [0; 32768],
};

/// PCB中与架构相关的信息
#[derive(Debug)]
#[allow(dead_code)]
pub struct ArchPCBInfo {
    rflags: usize,
    rbx: usize,
    r12: usize,
    r13: usize,
    r14: usize,
    r15: usize,
    rbp: usize,
    rsp: usize,
    rip: usize,
    cr2: usize,
    fsbase: usize,
    gsbase: usize,
    fs: SegmentSelector,
    gs: SegmentSelector,
    /// 存储PCB系统调用栈以及在syscall过程中暂存用户态rsp的结构体
    gsdata: X86_64GSData,
    /// 浮点寄存器的状态
    fp_state: Option<FpState>,
}

#[allow(dead_code)]
impl ArchPCBInfo {
    /// 创建一个新的ArchPCBInfo
    ///
    /// ## 参数
    ///
    /// - `kstack`：内核栈的引用，如果为None，则不会设置rsp和rbp。如果为Some，则会设置rsp和rbp为内核栈的最高地址。
    ///
    /// ## 返回值
    ///
    /// 返回一个新的ArchPCBInfo
    #[inline(never)]
    pub fn new(kstack: &KernelStack) -> Self {
        let mut r = Self {
            rflags: 0,
            rbx: 0,
            r12: 0,
            r13: 0,
            r14: 0,
            r15: 0,
            rbp: 0,
            rsp: 0,
            rip: 0,
            cr2: 0,
            fsbase: 0,
            gsbase: 0,
            gsdata: X86_64GSData {
                kaddr: VirtAddr::new(0),
                uaddr: VirtAddr::new(0),
            },
            fs: KERNEL_DS,
            gs: KERNEL_DS,
            fp_state: None,
        };

        r.rsp = kstack.stack_max_address().data() - 8;
        r.rbp = kstack.stack_max_address().data();

        return r;
    }

    pub fn set_stack(&mut self, stack: VirtAddr) {
        self.rsp = stack.data();
    }

    pub fn set_stack_base(&mut self, stack_base: VirtAddr) {
        self.rbp = stack_base.data();
    }

    pub fn rbp(&self) -> usize {
        self.rbp
    }

    pub unsafe fn push_to_stack(&mut self, value: usize) {
        self.rsp -= core::mem::size_of::<usize>();
        *(self.rsp as *mut usize) = value;
    }

    pub unsafe fn pop_from_stack(&mut self) -> usize {
        let value = *(self.rsp as *const usize);
        self.rsp += core::mem::size_of::<usize>();
        value
    }

    pub fn save_fp_state(&mut self) {
        if self.fp_state.is_none() {
            self.fp_state = Some(FpState::new());
        }

        self.fp_state.as_mut().unwrap().save();
    }

    pub fn restore_fp_state(&mut self) {
        if unlikely(self.fp_state.is_none()) {
            return;
        }

        self.fp_state.as_mut().unwrap().restore();
    }

    /// 返回浮点寄存器结构体的副本
    pub fn fp_state(&self) -> &Option<FpState> {
        &self.fp_state
    }

    // 清空浮点寄存器
    pub fn clear_fp_state(&mut self) {
        if unlikely(self.fp_state.is_none()) {
            warn!("fp_state is none");
            return;
        }

        self.fp_state.as_mut().unwrap().clear();
    }
    pub unsafe fn save_fsbase(&mut self) {
        if x86::controlregs::cr4().contains(Cr4::CR4_ENABLE_FSGSBASE) {
            self.fsbase = x86::current::segmentation::rdfsbase() as usize;
        } else {
            self.fsbase = x86::msr::rdmsr(x86::msr::IA32_FS_BASE) as usize;
        }
    }

    pub unsafe fn save_gsbase(&mut self) {
        if x86::controlregs::cr4().contains(Cr4::CR4_ENABLE_FSGSBASE) {
            self.gsbase = x86::current::segmentation::rdgsbase() as usize;
        } else {
            self.gsbase = x86::msr::rdmsr(x86::msr::IA32_GS_BASE) as usize;
        }
    }

    pub unsafe fn restore_fsbase(&mut self) {
        if x86::controlregs::cr4().contains(Cr4::CR4_ENABLE_FSGSBASE) {
            x86::current::segmentation::wrfsbase(self.fsbase as u64);
        } else {
            x86::msr::wrmsr(x86::msr::IA32_FS_BASE, self.fsbase as u64);
        }
    }

    pub unsafe fn restore_gsbase(&mut self) {
        if x86::controlregs::cr4().contains(Cr4::CR4_ENABLE_FSGSBASE) {
            x86::current::segmentation::wrgsbase(self.gsbase as u64);
        } else {
            x86::msr::wrmsr(x86::msr::IA32_GS_BASE, self.gsbase as u64);
        }
    }

    /// 将gsdata写入KernelGsbase寄存器
    pub unsafe fn store_kernel_gsbase(&self) {
        x86::msr::wrmsr(
            x86::msr::IA32_KERNEL_GSBASE,
            &self.gsdata as *const X86_64GSData as u64,
        );
    }

    /// ### 初始化系统调用栈，不得与PCB内核栈冲突(即传入的应该是一个新的栈，避免栈损坏)
    pub fn init_syscall_stack(&mut self, stack: &KernelStack) {
        self.gsdata.set_kstack(stack.stack_max_address() - 8);
    }

    pub fn fsbase(&self) -> usize {
        self.fsbase
    }

    pub fn gsbase(&self) -> usize {
        self.gsbase
    }

    pub fn cr2_mut(&mut self) -> &mut usize {
        &mut self.cr2
    }

    pub fn fp_state_mut(&mut self) -> &mut Option<FpState> {
        &mut self.fp_state
    }

    /// ### 克隆ArchPCBInfo,需要注意gsdata也是对应clone的
    pub fn clone_all(&self) -> Self {
        Self {
            rflags: self.rflags,
            rbx: self.rbx,
            r12: self.r12,
            r13: self.r13,
            r14: self.r14,
            r15: self.r15,
            rbp: self.rbp,
            rsp: self.rsp,
            rip: self.rip,
            cr2: self.cr2,
            fsbase: self.fsbase,
            gsbase: self.gsbase,
            fs: self.fs,
            gs: self.gs,
            gsdata: self.gsdata.clone(),
            fp_state: self.fp_state,
        }
    }

    // ### 从另一个ArchPCBInfo处clone,gsdata会被保留
    pub fn clone_from(&mut self, from: &Self) {
        let gsdata = self.gsdata.clone();
        *self = from.clone_all();
        self.gsdata = gsdata;
    }
}

impl ProcessControlBlock {
    /// 获取当前进程的pcb
    pub fn arch_current_pcb() -> Arc<Self> {
        // 获取栈指针
        let ptr = VirtAddr::new(x86::current::registers::rsp() as usize);

        let stack_base = VirtAddr::new(ptr.data() & (!(KernelStack::ALIGN - 1)));

        // 从内核栈的最低地址处取出pcb的地址
        let p = stack_base.data() as *const *const ProcessControlBlock;
        if unlikely((unsafe { *p }).is_null()) {
            error!("p={:p}", p);
            panic!("current_pcb is null");
        }
        unsafe {
            // 为了防止内核栈的pcb weak 指针被释放，这里需要将其包装一下
            let weak_wrapper: ManuallyDrop<Weak<ProcessControlBlock>> =
                ManuallyDrop::new(Weak::from_raw(*p));

            let new_arc: Arc<ProcessControlBlock> = weak_wrapper.upgrade().unwrap();
            return new_arc;
        }
    }
}

impl ProcessManager {
    pub fn arch_init() {
        // do nothing
    }
    /// fork的过程中复制线程
    ///
    /// 由于这个过程与具体的架构相关，所以放在这里
    pub fn copy_thread(
        current_pcb: &Arc<ProcessControlBlock>,
        new_pcb: &Arc<ProcessControlBlock>,
        clone_args: &KernelCloneArgs,
        current_trapframe: &TrapFrame,
    ) -> Result<(), SystemError> {
        let clone_flags = clone_args.flags;
        let mut child_trapframe = *current_trapframe;

        // 子进程的返回值为0
        child_trapframe.set_return_value(0);

        // 设置子进程的栈基址（开始执行中断返回流程时的栈基址）
        let mut new_arch_guard = unsafe { new_pcb.arch_info() };
        let kernel_stack_guard = new_pcb.kernel_stack();

        // 设置子进程在内核态开始执行时的rsp、rbp
        new_arch_guard.set_stack_base(kernel_stack_guard.stack_max_address());

        let trap_frame_vaddr: VirtAddr =
            kernel_stack_guard.stack_max_address() - core::mem::size_of::<TrapFrame>();
        new_arch_guard.set_stack(trap_frame_vaddr);

        // 拷贝栈帧
        unsafe {
            let usp = clone_args.stack;
            if usp != 0 {
                child_trapframe.rsp = usp as u64;
            }
            let trap_frame_ptr = trap_frame_vaddr.data() as *mut TrapFrame;
            *trap_frame_ptr = child_trapframe;
        }

        let current_arch_guard = current_pcb.arch_info_irqsave();
        new_arch_guard.fsbase = current_arch_guard.fsbase;
        new_arch_guard.gsbase = current_arch_guard.gsbase;
        new_arch_guard.fs = current_arch_guard.fs;
        new_arch_guard.gs = current_arch_guard.gs;
        new_arch_guard.fp_state = current_arch_guard.fp_state;

        // 拷贝浮点寄存器的状态
        if let Some(fp_state) = current_arch_guard.fp_state.as_ref() {
            new_arch_guard.fp_state = Some(*fp_state);
        }
        drop(current_arch_guard);

        // 设置返回地址（子进程开始执行的指令地址）
        if new_pcb.flags().contains(ProcessFlags::KTHREAD) {
            let kthread_bootstrap_stage1_func_addr = kernel_thread_bootstrap_stage1 as usize;
            new_arch_guard.rip = kthread_bootstrap_stage1_func_addr;
        } else {
            new_arch_guard.rip = ret_from_intr as usize;
        }

        // 设置tls
        if clone_flags.contains(CloneFlags::CLONE_SETTLS) {
            drop(new_arch_guard);
            Syscall::do_arch_prctl_64(new_pcb, ARCH_SET_FS, clone_args.tls, true)?;
        }

        return Ok(());
    }

    /// 切换进程
    ///
    /// ## 参数
    ///
    /// - `prev`：上一个进程的pcb
    /// - `next`：下一个进程的pcb
    pub unsafe fn switch_process(prev: Arc<ProcessControlBlock>, next: Arc<ProcessControlBlock>) {
        assert!(!CurrentIrqArch::is_irq_enabled());

        // 保存浮点寄存器
        prev.arch_info_irqsave().save_fp_state();
        // 切换浮点寄存器
        next.arch_info_irqsave().restore_fp_state();

        // 切换fsbase
        prev.arch_info_irqsave().save_fsbase();
        next.arch_info_irqsave().restore_fsbase();

        // 切换gsbase
        Self::switch_gsbase(&prev, &next);

        // 切换地址空间
        let next_addr_space = next.basic().user_vm().as_ref().unwrap().clone();
        compiler_fence(Ordering::SeqCst);

        next_addr_space.read().user_mapper.utable.make_current();
        drop(next_addr_space);
        compiler_fence(Ordering::SeqCst);
        // 切换内核栈

        // 获取arch info的锁，并强制泄露其守卫（切换上下文后，在switch_finish_hook中会释放锁）
        let next_arch = SpinLockGuard::leak(next.arch_info_irqsave()) as *mut ArchPCBInfo;
        let prev_arch = SpinLockGuard::leak(prev.arch_info_irqsave()) as *mut ArchPCBInfo;

        (*prev_arch).rip = switch_back as usize;

        // 恢复当前的 preempt count*2
        ProcessManager::current_pcb().preempt_enable();
        ProcessManager::current_pcb().preempt_enable();

        // 切换tss
        TSSManager::current_tss().set_rsp(
            x86::Ring::Ring0,
            next.kernel_stack().stack_max_address().data() as u64,
        );
        PROCESS_SWITCH_RESULT.as_mut().unwrap().get_mut().prev_pcb = Some(prev);
        PROCESS_SWITCH_RESULT.as_mut().unwrap().get_mut().next_pcb = Some(next);
        // debug!("switch tss ok");
        compiler_fence(Ordering::SeqCst);
        // 正式切换上下文
        switch_to_inner(prev_arch, next_arch);
    }

    unsafe fn switch_gsbase(prev: &Arc<ProcessControlBlock>, next: &Arc<ProcessControlBlock>) {
        asm!("swapgs", options(nostack, preserves_flags));
        prev.arch_info_irqsave().save_gsbase();
        next.arch_info_irqsave().restore_gsbase();
        // 将下一个进程的kstack写入kernel_gsbase
        next.arch_info_irqsave().store_kernel_gsbase();
        asm!("swapgs", options(nostack, preserves_flags));
    }
}

/// 保存上下文，然后切换进程，接着jmp到`switch_finish_hook`钩子函数
#[naked]
unsafe extern "sysv64" fn switch_to_inner(prev: *mut ArchPCBInfo, next: *mut ArchPCBInfo) {
    core::arch::naked_asm!(
        // As a quick reminder for those who are unfamiliar with the System V ABI (extern "C"):
        //
        // - the current parameters are passed in the registers `rdi`, `rsi`,
        // - we can modify scratch registers, e.g. rax
        // - we cannot change callee-preserved registers arbitrarily, e.g. rbx, which is why we
        //   store them here in the first place.
        concat!("
        // Save old registers, and load new ones
        mov [rdi + {off_rbx}], rbx
        mov rbx, [rsi + {off_rbx}]

        mov [rdi + {off_r12}], r12
        mov r12, [rsi + {off_r12}]

        mov [rdi + {off_r13}], r13
        mov r13, [rsi + {off_r13}]

        mov [rdi + {off_r14}], r14
        mov r14, [rsi + {off_r14}]

        mov [rdi + {off_r15}], r15
        mov r15, [rsi + {off_r15}]

        // switch segment registers (这些寄存器只能通过接下来的switch_hook的return来切换)
        mov [rdi + {off_fs}], fs
        mov [rdi + {off_gs}], gs

        // mov fs, [rsi + {off_fs}]
        // mov gs, [rsi + {off_gs}]

        mov [rdi + {off_rbp}], rbp
        mov rbp, [rsi + {off_rbp}]

        mov [rdi + {off_rsp}], rsp
        mov rsp, [rsi + {off_rsp}]

        // // push RFLAGS (can only be modified via stack)
        pushfq
        // // pop RFLAGS into `self.rflags`
        pop QWORD PTR [rdi + {off_rflags}]

        // // push `next.rflags`
        push QWORD PTR [rsi + {off_rflags}]
        // // pop into RFLAGS
        popfq

        // push next rip to stack
        push QWORD PTR [rsi + {off_rip}]


        // When we return, we cannot even guarantee that the return address on the stack, points to
        // the calling function. Thus, we have to execute this Rust hook by
        // ourselves, which will unlock the contexts before the later switch.

        // Note that switch_finish_hook will be responsible for executing `ret`.
        jmp {switch_hook}
        "),

        off_rflags = const(offset_of!(ArchPCBInfo, rflags)),

        off_rbx = const(offset_of!(ArchPCBInfo, rbx)),
        off_r12 = const(offset_of!(ArchPCBInfo, r12)),
        off_r13 = const(offset_of!(ArchPCBInfo, r13)),
        off_r14 = const(offset_of!(ArchPCBInfo, r14)),
        off_rbp = const(offset_of!(ArchPCBInfo, rbp)),
        off_rsp = const(offset_of!(ArchPCBInfo, rsp)),
        off_r15 = const(offset_of!(ArchPCBInfo, r15)),
        off_rip = const(offset_of!(ArchPCBInfo, rip)),
        off_fs = const(offset_of!(ArchPCBInfo, fs)),
        off_gs = const(offset_of!(ArchPCBInfo, gs)),

        switch_hook = sym crate::process::switch_finish_hook,
    );
}

#[naked]
unsafe extern "sysv64" fn switch_back() -> ! {
    core::arch::naked_asm!("ret");
}

pub unsafe fn arch_switch_to_user(trap_frame: TrapFrame) -> ! {
    // 以下代码不能发生中断
    CurrentIrqArch::interrupt_disable();

    let current_pcb = ProcessManager::current_pcb();
    let trap_frame_vaddr = VirtAddr::new(
        current_pcb.kernel_stack().stack_max_address().data() - core::mem::size_of::<TrapFrame>(),
    );
    // debug!("trap_frame_vaddr: {:?}", trap_frame_vaddr);

    assert!(
        (x86::current::registers::rsp() as usize) < trap_frame_vaddr.data(),
        "arch_switch_to_user(): current_rsp >= fake trap 
        frame vaddr, this may cause some illegal access to memory! 
        rsp: {:#x}, trap_frame_vaddr: {:#x}",
        x86::current::registers::rsp() as usize,
        trap_frame_vaddr.data()
    );

    let new_rip = VirtAddr::new(ret_from_intr as usize);
    let mut arch_guard = current_pcb.arch_info_irqsave();
    arch_guard.rsp = trap_frame_vaddr.data();

    arch_guard.fs = USER_DS;
    arch_guard.gs = USER_DS;

    // 将内核gs数据压进cpu
    arch_guard.store_kernel_gsbase();

    switch_fs_and_gs(
        SegmentSelector::from_bits_truncate(arch_guard.fs.bits()),
        SegmentSelector::from_bits_truncate(arch_guard.gs.bits()),
    );
    arch_guard.rip = new_rip.data();

    drop(arch_guard);

    drop(current_pcb);
    compiler_fence(Ordering::SeqCst);

    // 重要！在这里之后，一定要保证上面的引用计数变量、动态申请的变量、锁的守卫都被drop了，否则可能导致内存安全问题！

    compiler_fence(Ordering::SeqCst);
    ready_to_switch_to_user(trap_frame, trap_frame_vaddr.data(), new_rip.data());
}

/// 由于需要依赖ret来切换到用户态，所以不能inline
#[inline(never)]
unsafe extern "sysv64" fn ready_to_switch_to_user(
    trap_frame: TrapFrame,
    trapframe_vaddr: usize,
    new_rip: usize,
) -> ! {
    *(trapframe_vaddr as *mut TrapFrame) = trap_frame;
    compiler_fence(Ordering::SeqCst);
    asm!(
        "swapgs",
        "mov rsp, {trapframe_vaddr}",
        "push {new_rip}",
        "ret",
        trapframe_vaddr = in(reg) trapframe_vaddr,
        new_rip = in(reg) new_rip
    );
    unreachable!()
}

// bitflags! {
//     pub struct ProcessThreadFlags: u32 {
//     /*
//     * thread information flags
//     * - these are process state flags that various assembly files
//     *   may need to access
//     */
//     const TIF_NOTIFY_RESUME	= 1 << 1;	/* callback before returning to user */
//     const TIF_SIGPENDING	=	1 << 2;	/* signal pending */
//     const TIF_NEED_RESCHED	= 1 << 3;	/* rescheduling necessary */
//     const TIF_SINGLESTEP	=	1 << 4;	/* reenable singlestep on user return*/
//     const TIF_SSBD		= 1 << 5;	/* Speculative store bypass disable */
//     const TIF_SPEC_IB		= 1 << 9;	/* Indirect branch speculation mitigation */
//     const TIF_SPEC_L1D_FLUSH	= 1 << 10;	/* Flush L1D on mm switches (processes) */
//     const TIF_USER_RETURN_NOTIFY	= 1 << 11;	/* notify kernel of userspace return */
//     const TIF_UPROBE		= 1 << 12;	/* breakpointed or singlestepping */
//     const TIF_PATCH_PENDING	= 1 << 13;	/* pending live patching update */
//     const TIF_NEED_FPU_LOAD	= 1 << 14;	/* load FPU on return to userspace */
//     const TIF_NOCPUID		= 1 << 15;	/* CPUID is not accessible in userland */
//     const TIF_NOTSC		= 1 << 16;	/* TSC is not accessible in userland */
//     const TIF_NOTIFY_SIGNAL	= 1 << 17;	/* signal notifications exist */
//     const TIF_MEMDIE		= 1 << 20;	/* is terminating due to OOM killer */
//     const TIF_POLLING_NRFLAG	= 1 << 21;	/* idle is polling for TIF_NEED_RESCHED */
//     const TIF_IO_BITMAP		= 1 << 22;	/* uses I/O bitmap */
//     const TIF_SPEC_FORCE_UPDATE	= 1 << 23;	/* Force speculation MSR update in context switch */
//     const TIF_FORCED_TF		= 1 << 24;	/* true if TF in eflags artificially */
//     const TIF_BLOCKSTEP		= 1 << 25;	/* set when we want DEBUGCTLMSR_BTF */
//     const TIF_LAZY_MMU_UPDATES	= 1 << 27;	/* task is updating the mmu lazily */
//     const TIF_ADDR32		= 1 << 29;	/* 32-bit address space on 64 bits */
//     }
// }

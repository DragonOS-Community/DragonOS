#![allow(function_casts_as_integer)]

use core::{
    arch::asm,
    intrinsics::unlikely,
    mem::ManuallyDrop,
    sync::atomic::{compiler_fence, AtomicU64, Ordering},
};

use alloc::sync::{Arc, Weak};

use kdepends::memoffset::offset_of;
use log::{error, info, warn};
use system_error::SystemError;
use x86::{controlregs::Cr4, segmentation::SegmentSelector};

use crate::{
    arch::process::table::TSSManager,
    exception::InterruptArch,
    libs::spinlock::{SpinLock, SpinLockGuard},
    mm::{percpu::PerCpu, VirtAddr},
    process::{
        fork::{CloneFlags, KernelCloneArgs},
        KernelStack, ProcessControlBlock, ProcessFlags, ProcessManager, PROCESS_SWITCH_RESULT,
    },
    smp::cpu::smp_cpu_manager,
    syscall::Syscall,
};

use self::{
    io_bitmap::TaskIoBitmap,
    kthread::kernel_thread_bootstrap_stage1,
    syscall::ARCH_SET_FS,
    table::{switch_fs_and_gs, KERNEL_DS, USER_DS},
};

use super::{
    cpu::current_cpu_id, driver::apic::CurrentApic, fpu::FpState, interrupt::TrapFrame,
    syscall::X86_64GSData, CurrentIrqArch,
};

pub mod idle;
pub mod io_bitmap;
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

/// x86 debug register primitives (ptrace hardware breakpoints).
///
/// DR0-3/DR7 are loaded per task by the context switch.
pub mod debugreg {
    /// Write the given debug register (DR0-3/DR7 only).
    #[inline]
    pub unsafe fn write_dr(n: usize, val: u64) {
        match n {
            0 => unsafe { core::arch::asm!("mov dr0, {0}", in(reg) val, options(nomem, nostack)) },
            1 => unsafe { core::arch::asm!("mov dr1, {0}", in(reg) val, options(nomem, nostack)) },
            2 => unsafe { core::arch::asm!("mov dr2, {0}", in(reg) val, options(nomem, nostack)) },
            3 => unsafe { core::arch::asm!("mov dr3, {0}", in(reg) val, options(nomem, nostack)) },
            7 => unsafe { core::arch::asm!("mov dr7, {0}", in(reg) val, options(nomem, nostack)) },
            _ => {}
        }
    }
    /// Write DR7. The write is serializing; call only when necessary.
    #[inline]
    pub unsafe fn write_dr7(val: u64) {
        unsafe { core::arch::asm!("mov dr7, {0}", in(reg) val, options(nomem, nostack)) }
    }
}

/// Per-CPU shadow of the current hardware DR7
static CPU_DR7: [AtomicU64; PerCpu::MAX_CPU_NUM as usize] =
    [const { AtomicU64::new(0) }; PerCpu::MAX_CPU_NUM as usize];
static CPU_DEBUG_OWNER_GENERATION: [AtomicU64; PerCpu::MAX_CPU_NUM as usize] =
    [const { AtomicU64::new(0) }; PerCpu::MAX_CPU_NUM as usize];

/// Per-CPU DR7 shadow slot; accessed only with IRQs off on the local CPU.
#[inline]
pub(crate) fn cpu_dr7() -> &'static AtomicU64 {
    &CPU_DR7[crate::smp::core::smp_get_processor_id().data() as usize]
}

#[inline]
pub(crate) fn cpu_debug_owner_generation() -> &'static AtomicU64 {
    &CPU_DEBUG_OWNER_GENERATION[crate::smp::core::smp_get_processor_id().data() as usize]
}

/// Restore the current task's latest debug-register state immediately before
/// returning to userspace. The pending flag keeps context switch from arming
/// DR7 while signal/ptrace work may schedule.
pub(crate) fn restore_current_debug_regs() {
    let _irq = unsafe { CurrentIrqArch::save_and_disable_irq() };
    let current = ProcessManager::current_pcb();
    current.flags().remove(ProcessFlags::PENDING_DEBUG);
    unsafe { load_task_debug_regs(&current) };
}

/// Clear local hardware state while exec replaces the current task image.
/// The saved task state and pending record have already been cleared.
pub(crate) fn clear_current_debug_regs(task: &ProcessControlBlock) {
    let _irq = unsafe { CurrentIrqArch::save_and_disable_irq() };
    unsafe { debugreg::write_dr7(0) };
    cpu_dr7().store(0, Ordering::Relaxed);
    cpu_debug_owner_generation().store(task.ptrace_session_generation(), Ordering::Relaxed);
}

unsafe fn load_task_debug_regs(task: &ProcessControlBlock) {
    let generation = task.ptrace_session_generation();
    cpu_debug_owner_generation().store(generation, Ordering::Relaxed);
    let should_load = task.flags().contains(ProcessFlags::HW_DEBUG_REGS)
        && !task.flags().contains(ProcessFlags::PENDING_DEBUG);
    if should_load {
        let mut dr = task.ptrace_debug_regs_snapshot();
        dr[6] = 0;
        dr[7] &= !crate::process::ptrace::DR_CONTROL_RESERVED;
        cpu_dr7().store(dr[7], Ordering::Relaxed);
        compiler_fence(Ordering::SeqCst);
        unsafe {
            debugreg::write_dr(0, dr[0]);
            debugreg::write_dr(1, dr[1]);
            debugreg::write_dr(2, dr[2]);
            debugreg::write_dr(3, dr[3]);
            debugreg::write_dr(7, dr[7]);
        }
    } else {
        unsafe { debugreg::write_dr7(0) };
        cpu_dr7().store(0, Ordering::Relaxed);
    }
}

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
    /// x86 I/O permission bitmap shared with forked user tasks.
    io_bitmap: Option<Arc<SpinLock<TaskIoBitmap>>>,
}

#[allow(dead_code)]
impl ArchPCBInfo {
    pub fn kernel_rsp(&self) -> usize {
        self.rsp
    }
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
            io_bitmap: None,
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
        // Linux 语义：新建线程/进程在首次运行用户态前必须拥有确定的初始 FPU 状态
        // （x87 控制字 / MXCSR 等），不能继承上一个任务的寄存器状态。
        if self.fp_state.is_none() {
            self.fp_state = Some(FpState::new());
        }

        self.fp_state.as_ref().unwrap().restore();
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
        } else if self.fs.bits() != 0 {
            // Without FSGSBASE a non-null selector supplies the hidden base
            // from its descriptor. Linux does not preserve a stale MSR base
            // for this legacy state.
            self.fsbase = 0;
        } else {
            self.fsbase = x86::msr::rdmsr(x86::msr::IA32_FS_BASE) as usize;
        }
    }

    pub(crate) unsafe fn save_fs_selector(&mut self) {
        let selector: u16;
        core::arch::asm!("mov {0:x}, fs", out(reg) selector, options(nostack, preserves_flags));
        self.fs = SegmentSelector::from_raw(selector);
    }

    pub(crate) unsafe fn save_gs_selector(&mut self) {
        let selector: u16;
        core::arch::asm!("mov {0:x}, gs", out(reg) selector, options(nostack, preserves_flags));
        self.gs = SegmentSelector::from_raw(selector);
    }

    /// Load a user-controlled selector with an exception-table fallback.
    /// Linux accepts any selector whose low 16 bits are zero or have RPL 3;
    /// a missing GDT/LDT descriptor is therefore handled at load time by
    /// clearing the hardware selector instead of faulting in kernel mode.
    pub(crate) unsafe fn restore_fs_selector(&mut self) {
        let selector = Self::load_fs_selector_with_fixup(self.fs.bits());
        self.fs = SegmentSelector::from_raw(selector);
    }

    pub(crate) unsafe fn restore_gs_selector(&mut self) {
        let selector = Self::load_gs_selector_with_fixup(self.gs.bits());
        self.gs = SegmentSelector::from_raw(selector);
    }

    pub(crate) unsafe fn load_fs_selector_with_fixup(mut selector: u16) -> u16 {
        core::arch::asm!(
            "2: mov fs, {selector:x}",
            "jmp 3f",
            "4: mov {selector:x}, 0",
            "mov fs, {selector:x}",
            "3:",
            ".pushsection __ex_table, \"a\"",
            ".balign 8",
            ".quad 2b - .",
            ".quad 4b - . + 8",
            ".popsection",
            selector = inout(reg) selector,
            options(nostack, preserves_flags)
        );
        selector
    }

    pub(crate) unsafe fn load_gs_selector_with_fixup(mut selector: u16) -> u16 {
        core::arch::asm!(
            "2: mov gs, {selector:x}",
            "jmp 3f",
            "4: mov {selector:x}, 0",
            "mov gs, {selector:x}",
            "3:",
            ".pushsection __ex_table, \"a\"",
            ".balign 8",
            ".quad 2b - .",
            ".quad 4b - . + 8",
            ".popsection",
            selector = inout(reg) selector,
            options(nostack, preserves_flags)
        );
        selector
    }

    /// Restore the user GS selector while kernel GS is active.
    ///
    /// `mov gs, selector` changes the active hidden GS base. Kernel entry code
    /// runs with the per-CPU GS base active, so mirror Linux's load_gs_index()
    /// and modify the inactive user side between a balanced swapgs pair.
    pub(crate) unsafe fn load_user_gs_selector_with_fixup(selector: u16) -> u16 {
        let irq_guard = CurrentIrqArch::save_and_disable_irq();
        core::arch::asm!("swapgs", options(nostack, preserves_flags));
        let selector = Self::load_gs_selector_with_fixup(selector);
        core::arch::asm!("swapgs", options(nostack, preserves_flags));
        drop(irq_guard);
        selector
    }

    pub(crate) unsafe fn load_ds_selector_with_fixup(mut selector: u16) -> u16 {
        core::arch::asm!(
            "2: mov ds, {selector:x}",
            "jmp 3f",
            "4: mov {selector:x}, 0",
            "mov ds, {selector:x}",
            "3:",
            ".pushsection __ex_table, \"a\"",
            ".balign 8",
            ".quad 2b - .",
            ".quad 4b - . + 8",
            ".popsection",
            selector = inout(reg) selector,
            options(nostack, preserves_flags)
        );
        selector
    }

    pub(crate) unsafe fn load_es_selector_with_fixup(mut selector: u16) -> u16 {
        core::arch::asm!(
            "2: mov es, {selector:x}",
            "jmp 3f",
            "4: mov {selector:x}, 0",
            "mov es, {selector:x}",
            "3:",
            ".pushsection __ex_table, \"a\"",
            ".balign 8",
            ".quad 2b - .",
            ".quad 4b - . + 8",
            ".popsection",
            selector = inout(reg) selector,
            options(nostack, preserves_flags)
        );
        selector
    }

    pub unsafe fn save_gsbase(&mut self) {
        if x86::controlregs::cr4().contains(Cr4::CR4_ENABLE_FSGSBASE) {
            self.gsbase = x86::current::segmentation::rdgsbase() as usize;
        } else if self.gs.bits() != 0 {
            self.gsbase = 0;
        } else {
            self.gsbase = x86::msr::rdmsr(x86::msr::IA32_GS_BASE) as usize;
        }
    }

    /// Save user GS base while in kernel context (after swapgs)
    pub unsafe fn save_user_gsbase(&mut self) {
        if !x86::controlregs::cr4().contains(Cr4::CR4_ENABLE_FSGSBASE) && self.gs.bits() != 0 {
            self.gsbase = 0;
        } else {
            self.gsbase = x86::msr::rdmsr(x86::msr::IA32_KERNEL_GSBASE) as usize;
        }
    }

    /// Write user GS base from kernel context: goes to IA32_KERNEL_GSBASE
    pub unsafe fn restore_user_gsbase(&mut self) {
        x86::msr::wrmsr(x86::msr::IA32_KERNEL_GSBASE, self.gsbase as u64);
    }

    pub unsafe fn restore_fsbase(&mut self) {
        if x86::controlregs::cr4().contains(Cr4::CR4_ENABLE_FSGSBASE) {
            x86::current::segmentation::wrfsbase(self.fsbase as u64);
        } else {
            x86::msr::wrmsr(x86::msr::IA32_FS_BASE, self.fsbase as u64);
        }
    }

    /// Clear the FS segment selector (hardware register and field)
    pub unsafe fn load_fs_selector_zero(&mut self) {
        self.fs = SegmentSelector::new(0, x86::Ring::Ring0);
        unsafe {
            core::arch::asm!("mov fs, {0:x}", in(reg) 0u16, options(nostack, preserves_flags))
        };
    }

    /// Clear the GS segment selector
    pub unsafe fn load_gs_selector_zero(&mut self) {
        self.gs = SegmentSelector::new(0, x86::Ring::Ring0);
        unsafe {
            core::arch::asm!("mov gs, {0:x}", in(reg) 0u16, options(nostack, preserves_flags))
        };
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

    /// Base exposed for a stopped task through ptrace. On legacy CPUs a
    /// non-null selector owns the hidden base; DragonOS has no per-task TLS
    /// GDT or LDT descriptors, so every such descriptor base is zero.
    pub(crate) fn ptrace_fsbase(&self) -> usize {
        if unsafe { x86::controlregs::cr4() }.contains(Cr4::CR4_ENABLE_FSGSBASE)
            || self.fs.bits() == 0
        {
            self.fsbase
        } else {
            0
        }
    }

    pub(crate) fn ptrace_gsbase(&self) -> usize {
        if unsafe { x86::controlregs::cr4() }.contains(Cr4::CR4_ENABLE_FSGSBASE)
            || self.gs.bits() == 0
        {
            self.gsbase
        } else {
            0
        }
    }

    pub(crate) fn fs_selector(&self) -> u16 {
        self.fs.bits()
    }

    pub(crate) fn gs_selector(&self) -> u16 {
        self.gs.bits()
    }

    /// Set user FS base (ptrace SETREGS write path).
    pub fn set_fsbase(&mut self, base: usize) {
        self.fsbase = base;
    }

    /// Set user GS base (same semantics as set_fsbase).
    pub fn set_gsbase(&mut self, base: usize) {
        self.gsbase = base;
    }

    pub(crate) fn set_fs_selector(&mut self, selector: u16) {
        self.fs = SegmentSelector::from_raw(selector);
    }

    pub(crate) fn set_gs_selector(&mut self, selector: u16) {
        self.gs = SegmentSelector::from_raw(selector);
    }

    pub fn cr2_mut(&mut self) -> &mut usize {
        &mut self.cr2
    }

    pub fn fp_state_mut(&mut self) -> &mut Option<FpState> {
        &mut self.fp_state
    }

    pub fn io_bitmap(&self) -> Option<Arc<SpinLock<TaskIoBitmap>>> {
        self.io_bitmap.clone()
    }

    pub fn io_bitmap_ref(&self) -> Option<&Arc<SpinLock<TaskIoBitmap>>> {
        self.io_bitmap.as_ref()
    }

    pub fn set_io_bitmap(&mut self, bitmap: Option<Arc<SpinLock<TaskIoBitmap>>>) {
        self.io_bitmap = bitmap;
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
            fp_state: self.fp_state,
            gsdata: self.gsdata.clone(),
            io_bitmap: self.io_bitmap.clone(),
        }
    }

    pub fn sync_current_state_before_fork(&mut self) {
        unsafe {
            self.save_fs_selector();
            self.save_fsbase();
            self.save_gs_selector();
            // fork runs in syscall context (after swapgs), so the true user gsbase
            // lives in IA32_KERNEL_GSBASE; save_user_gsbase is required.
            self.save_user_gsbase();
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
            if clone_args.stack != 0 {
                // stack 是栈的顶部（低地址），栈底需要加上 stack_size
                // 注意：x86_64 栈向下增长，所以 rsp 应该指向高地址
                let stack_top = clone_args.stack + clone_args.stack_size;
                child_trapframe.rsp = stack_top as u64;
            }
            let trap_frame_ptr = trap_frame_vaddr.data() as *mut TrapFrame;
            *trap_frame_ptr = child_trapframe;
        }

        let mut current_arch_guard = current_pcb.arch_info_irqsave();

        // Copy the parent's arch info
        // Note: the guard must be mut to save FP state
        unsafe {
            current_arch_guard.save_fs_selector();
            current_arch_guard.save_fsbase();
            current_arch_guard.save_gs_selector();
            // Synchronous fork: read the true user gsbase from IA32_KERNEL_GSBASE
            current_arch_guard.save_user_gsbase();
        }

        // 在拷贝FP状态之前，先从硬件寄存器保存当前的FP状态
        // 这样确保即使在信号处理函数中fork，子进程也能继承fork时刻的真实FP寄存器状态
        current_arch_guard.save_fp_state();

        new_arch_guard.fsbase = current_arch_guard.fsbase;
        new_arch_guard.gsbase = current_arch_guard.gsbase;
        new_arch_guard.fs = current_arch_guard.fs;
        new_arch_guard.gs = current_arch_guard.gs;
        new_arch_guard.fp_state = current_arch_guard.fp_state;
        new_arch_guard.io_bitmap = current_arch_guard.io_bitmap.clone();

        // 拷贝浮点寄存器的状态
        if let Some(fp_state) = current_arch_guard.fp_state.as_ref() {
            new_arch_guard.fp_state = Some(*fp_state);
        }
        drop(current_arch_guard);

        // 设置返回地址（子进程开始执行的指令地址）
        if new_pcb.flags().contains(ProcessFlags::KTHREAD) {
            new_arch_guard.io_bitmap = None;
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

        // Loading a selector can replace its hidden base. Preserve Linux's
        // selector-before-base ordering when installing the next FS state.
        let prev_fs_selector = {
            let mut prev_arch = prev.arch_info_irqsave();
            prev_arch.save_fs_selector();
            prev_arch.save_fsbase();
            prev_arch.fs_selector()
        };
        {
            let mut next_arch = next.arch_info_irqsave();
            let requested_fs_selector = next_arch.fs_selector();
            if prev_fs_selector != 0 || requested_fs_selector != 0 {
                next_arch.restore_fs_selector();
            }
            if x86::controlregs::cr4().contains(Cr4::CR4_ENABLE_FSGSBASE)
                || requested_fs_selector <= 3
            {
                next_arch.restore_fsbase();
            }
        }

        // 切换gsbase
        Self::switch_gsbase(&prev, &next);

        // Switch hardware debug registers (ptrace POKEUSER breakpoints):
        // a task with HW_DEBUG_REGS loads its DR0-3/DR7 on switch-in.
        unsafe { load_task_debug_regs(&next) };

        // 切换地址空间（无锁快速路径）
        let next_addr_space = next.basic().user_vm();
        let prev_user_vm = prev.basic().user_vm();
        let prev_active_mm = prev_user_vm
            .clone()
            .or_else(crate::mm::tlb::tlb_state_loaded_mm);
        let cpu = crate::smp::core::smp_get_processor_id();
        compiler_fence(Ordering::SeqCst);

        // INV-1: before loading a different user mm, mark this CPU active in the
        // next mm so concurrent remote shootdowns cannot miss the CPU after CR3
        // changes. Keep the previous bit until after the hardware switch; the
        // temporary double membership is safe and may only cause an extra IPI.
        // Order: set(next) → set CR3 → clear(prev) → update per-CPU TlbState.
        //
        // If next has no user mm, keep the current loaded mm in lazy-TLB mode.
        let same_mm = match (prev_active_mm.as_ref(), next_addr_space.as_ref()) {
            (Some(p), Some(n)) => Arc::ptr_eq(p, n),
            _ => false,
        };

        if let Some(next_mm) = next_addr_space {
            if !same_mm {
                next_mm.active_cpus_set(cpu);
            }

            next_mm.make_current();
            compiler_fence(Ordering::SeqCst);

            if !same_mm {
                if let Some(prev_mm) = prev_active_mm.as_ref() {
                    prev_mm.active_cpus_clear(cpu);
                }
            }
            // Update per-CPU TlbState: hardware-loaded mm and generation
            crate::mm::tlb::tlb_state_set_loaded_mm(next_mm.clone());
        }
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
        TSSManager::invalidate_io_bitmap();
        PROCESS_SWITCH_RESULT.as_mut().unwrap().get_mut().prev_pcb = Some(prev);
        PROCESS_SWITCH_RESULT.as_mut().unwrap().get_mut().next_pcb = Some(next);
        // debug!("switch tss ok");
        compiler_fence(Ordering::SeqCst);
        // 正式切换上下文
        switch_to_inner(prev_arch, next_arch);
    }

    unsafe fn switch_gsbase(prev: &Arc<ProcessControlBlock>, next: &Arc<ProcessControlBlock>) {
        asm!("swapgs", options(nostack, preserves_flags));
        let prev_gs_selector = {
            let mut prev_arch = prev.arch_info_irqsave();
            prev_arch.save_gs_selector();
            prev_arch.save_gsbase();
            prev_arch.gs_selector()
        };
        {
            let mut next_arch = next.arch_info_irqsave();
            let requested_gs_selector = next_arch.gs_selector();
            if prev_gs_selector != 0 || requested_gs_selector != 0 {
                next_arch.restore_gs_selector();
            }
            if x86::controlregs::cr4().contains(Cr4::CR4_ENABLE_FSGSBASE)
                || requested_gs_selector <= 3
            {
                next_arch.restore_gsbase();
            }
            // 将下一个进程的kstack写入kernel_gsbase
            next_arch.store_kernel_gsbase();
        }
        asm!("swapgs", options(nostack, preserves_flags));
    }
}

/// 保存上下文，然后切换进程，接着jmp到`switch_finish_hook`钩子函数
#[unsafe(naked)]
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
        switch_hook = sym crate::process::switch_finish_hook,
    );
}

#[unsafe(naked)]
unsafe extern "sysv64" fn switch_back() -> ! {
    core::arch::naked_asm!("ret");
}

pub unsafe fn arch_switch_to_user(trap_frame: TrapFrame) -> ! {
    // 以下代码不能发生中断
    CurrentIrqArch::interrupt_disable();

    // 确保在返回用户态之前，当前任务的 FPU/SSE 状态已被恢复。
    // 这对于“第一次进入用户态但还没发生过一次调度切换”的任务尤为关键。
    ProcessManager::current_pcb()
        .arch_info_irqsave()
        .restore_fp_state();

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
        SegmentSelector::from_raw(arch_guard.fs.bits()),
        SegmentSelector::from_raw(arch_guard.gs.bits()),
    );
    arch_guard.rip = new_rip.data();

    drop(arch_guard);

    drop(current_pcb);
    compiler_fence(Ordering::SeqCst);

    // 重要！在这里之后，一定要保证上面的引用计数变量、动态申请的变量、锁的守卫都被drop了，否则可能导致内存安全问题！

    compiler_fence(Ordering::SeqCst);
    TSSManager::update_io_bitmap_from_current();
    crate::rcu::note_exit_to_user_mode();
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

/// # 功能
///
/// 停止当前CPU的运行，系统进入最终的停机状态
pub(crate) fn stop_this_cpu() -> ! {
    let cpu_id = current_cpu_id();

    unsafe {
        CurrentIrqArch::interrupt_disable();
    }

    crate::rcu::cpu_offline(cpu_id);
    // 将当前cpu标记为offline
    smp_cpu_manager().set_online_cpu(cpu_id, false);
    CurrentApic.disable_local_apic();

    loop {
        native_halt();
    }
}

#[inline(always)]
fn native_halt() {
    info!("Starting System Halt...");
    unsafe {
        asm!("hlt", options(nomem, nostack, preserves_flags));
    }
}

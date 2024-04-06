use alloc::{
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    arch::asm,
    intrinsics::unlikely,
    mem::ManuallyDrop,
    sync::atomic::{compiler_fence, Ordering},
};
use kdepends::memoffset::offset_of;
use system_error::SystemError;

use crate::{
    arch::{
        interrupt::entry::ret_from_exception, process::kthread::kernel_thread_bootstrap_stage1,
        CurrentIrqArch,
    },
    exception::InterruptArch,
    kerror,
    libs::spinlock::SpinLockGuard,
    mm::VirtAddr,
    process::{
        fork::{CloneFlags, KernelCloneArgs},
        KernelStack, ProcessControlBlock, ProcessFlags, ProcessManager, PROCESS_SWITCH_RESULT,
    },
    smp::cpu::ProcessorId,
    syscall::Syscall,
};

use super::{
    cpu::{local_context, LocalContext},
    interrupt::TrapFrame,
};

pub mod idle;
pub mod kthread;
pub mod syscall;

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

pub unsafe fn arch_switch_to_user(path: String, argv: Vec<String>, envp: Vec<String>) -> ! {
    unimplemented!("RiscV64 arch_switch_to_user")
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
        clone_args: KernelCloneArgs,
        current_trapframe: &TrapFrame,
    ) -> Result<(), SystemError> {
        let clone_flags = clone_args.flags;
        let mut child_trapframe = *current_trapframe;

        // 子进程的返回值为0
        child_trapframe.set_return_value(0);

        // 设置子进程的栈基址（开始执行中断返回流程时的栈基址）
        let mut new_arch_guard = unsafe { new_pcb.arch_info() };
        let kernel_stack_guard = new_pcb.kernel_stack();
        let trap_frame_vaddr: VirtAddr =
            kernel_stack_guard.stack_max_address() - core::mem::size_of::<TrapFrame>();
        new_arch_guard.set_stack(trap_frame_vaddr);

        // 拷贝栈帧
        unsafe {
            let usp = clone_args.stack;
            if usp != 0 {
                child_trapframe.sp = usp;
            }
            let trap_frame_ptr = trap_frame_vaddr.data() as *mut TrapFrame;
            *trap_frame_ptr = child_trapframe;
        }

        // copy arch info

        let current_arch_guard = current_pcb.arch_info_irqsave();
        // 拷贝浮点寄存器的状态
        new_arch_guard.fp_state = current_arch_guard.fp_state;

        drop(current_arch_guard);

        // 设置返回地址（子进程开始执行的指令地址）
        if new_pcb.flags().contains(ProcessFlags::KTHREAD) {
            let kthread_bootstrap_stage1_func_addr = kernel_thread_bootstrap_stage1 as usize;
            new_arch_guard.ra = kthread_bootstrap_stage1_func_addr;
        } else {
            new_arch_guard.ra = ret_from_exception as usize;
        }

        // 设置tls
        if clone_flags.contains(CloneFlags::CLONE_SETTLS) {
            drop(new_arch_guard);
            todo!("set tls");
        }

        return Ok(());
    }

    /// 切换进程
    ///
    /// ## 参数
    ///
    /// - `prev`：上一个进程的pcb
    /// - `next`：下一个进程的pcb
    ///
    /// 参考: https://code.dragonos.org.cn/xref/linux-6.6.21/arch/riscv/include/asm/switch_to.h#76
    pub unsafe fn switch_process(prev: Arc<ProcessControlBlock>, next: Arc<ProcessControlBlock>) {
        assert!(!CurrentIrqArch::is_irq_enabled());
        Self::switch_process_fpu(&prev, &next);
        Self::switch_local_context(&prev, &next);

        // 切换地址空间
        let next_addr_space = next.basic().user_vm().as_ref().unwrap().clone();
        compiler_fence(Ordering::SeqCst);

        next_addr_space.read().user_mapper.utable.make_current();
        drop(next_addr_space);
        compiler_fence(Ordering::SeqCst);

        // 获取arch info的锁，并强制泄露其守卫（切换上下文后，在switch_finish_hook中会释放锁）
        let next_arch = SpinLockGuard::leak(next.arch_info_irqsave()) as *mut ArchPCBInfo;
        let prev_arch = SpinLockGuard::leak(prev.arch_info_irqsave()) as *mut ArchPCBInfo;

        // 恢复当前的 preempt count*2
        ProcessManager::current_pcb().preempt_enable();
        ProcessManager::current_pcb().preempt_enable();
        PROCESS_SWITCH_RESULT.as_mut().unwrap().get_mut().prev_pcb = Some(prev);
        PROCESS_SWITCH_RESULT.as_mut().unwrap().get_mut().next_pcb = Some(next);
        // kdebug!("switch tss ok");
        compiler_fence(Ordering::SeqCst);
        // 正式切换上下文
        switch_to_inner(prev_arch, next_arch);
    }

    fn switch_process_fpu(prev: &Arc<ProcessControlBlock>, next: &Arc<ProcessControlBlock>) {
        let prev_regs = unsafe { Self::task_trapframe(prev) };
        let next_regs = unsafe { Self::task_trapframe(next) };
        if unlikely(prev_regs.status.sd()) {
            prev.arch_info_irqsave().fp_state.save(prev_regs);
        }
        next.arch_info_irqsave().fp_state.restore(next_regs);
    }

    fn switch_local_context(prev: &Arc<ProcessControlBlock>, next: &Arc<ProcessControlBlock>) {
        prev.arch_info_irqsave().local_context = *local_context().get();
        local_context()
            .get_mut()
            .restore(&next.arch_info_irqsave().local_context);
    }

    unsafe fn task_trapframe(task: &Arc<ProcessControlBlock>) -> &mut TrapFrame {
        let mut sp = task.kernel_stack().stack_max_address().data();
        sp -= core::mem::size_of::<TrapFrame>();
        return (sp as *mut TrapFrame).as_mut().unwrap();
    }
}

/// 切换上下文
///
/// 参考 https://code.dragonos.org.cn/xref/linux-6.6.21/arch/riscv/kernel/entry.S#233
#[naked]
unsafe extern "C" fn switch_to_inner(prev: *mut ArchPCBInfo, next: *mut ArchPCBInfo) {
    core::arch::asm!(concat!(
        "
            sd ra, {off_ra}(a0)
            sd sp, {off_sp}(a0)
            sd s0, {off_s0}(a0)
            sd s1, {off_s1}(a0)
            sd s2, {off_s2}(a0)
            sd s3, {off_s3}(a0)
            sd s4, {off_s4}(a0)
            sd s5, {off_s5}(a0)
            sd s6, {off_s6}(a0)
            sd s7, {off_s7}(a0)
            sd s8, {off_s8}(a0)
            sd s9, {off_s9}(a0)
            sd s10, {off_s10}(a0)
            sd s11, {off_s11}(a0)


            ld sp, {off_sp}(a1)
            ld s0, {off_s0}(a1)
            ld s1, {off_s1}(a1)
            ld s2, {off_s2}(a1)
            ld s3, {off_s3}(a1)
            ld s4, {off_s4}(a1)
            ld s5, {off_s5}(a1)
            ld s6, {off_s6}(a1)
            ld s7, {off_s7}(a1)
            ld s8, {off_s8}(a1)
            ld s9, {off_s9}(a1)
            ld s10, {off_s10}(a1)
            ld s11, {off_s11}(a1)
            
            // 将ra设置为标签1，并跳转到{switch_finish_hook}
            la ra, 1f
            j {switch_finish_hook}
            
            1:
            ld sp, {off_sp}(a1)
            ld ra, {off_ra}(a1)
            ret

        "
    ), 
    off_ra = const(offset_of!(ArchPCBInfo, ra)),
    off_sp = const(offset_of!(ArchPCBInfo, ksp)),
    off_s0 = const(offset_of!(ArchPCBInfo, s0)),
    off_s1 = const(offset_of!(ArchPCBInfo, s1)),
    off_s2 = const(offset_of!(ArchPCBInfo, s2)),
    off_s3 = const(offset_of!(ArchPCBInfo, s3)),
    off_s4 = const(offset_of!(ArchPCBInfo, s4)),
    off_s5 = const(offset_of!(ArchPCBInfo, s5)),
    off_s6 = const(offset_of!(ArchPCBInfo, s6)),
    off_s7 = const(offset_of!(ArchPCBInfo, s7)),
    off_s8 = const(offset_of!(ArchPCBInfo, s8)),
    off_s9 = const(offset_of!(ArchPCBInfo, s9)),
    off_s10 = const(offset_of!(ArchPCBInfo, s10)),
    off_s11 = const(offset_of!(ArchPCBInfo, s11)),
    switch_finish_hook = sym crate::process::switch_finish_hook,
    options(noreturn));
}

impl ProcessControlBlock {
    /// 获取当前进程的pcb
    pub fn arch_current_pcb() -> Arc<Self> {
        // 获取栈指针
        let mut sp: usize;
        unsafe { asm!("mv {}, sp", lateout(reg) sp, options(nostack)) };
        let ptr = VirtAddr::new(sp);

        let stack_base = VirtAddr::new(ptr.data() & (!(KernelStack::ALIGN - 1)));

        // 从内核栈的最低地址处取出pcb的地址
        let p = stack_base.data() as *const *const ProcessControlBlock;
        if core::intrinsics::unlikely((unsafe { *p }).is_null()) {
            kerror!("p={:p}", p);
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

/// PCB中与架构相关的信息
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
#[repr(C)]
pub struct ArchPCBInfo {
    ra: usize,
    ksp: usize,
    s0: usize,
    s1: usize,
    s2: usize,
    s3: usize,
    s4: usize,
    s5: usize,
    s6: usize,
    s7: usize,
    s8: usize,
    s9: usize,
    s10: usize,
    s11: usize,

    fp_state: FpDExtState,
    local_context: LocalContext,
}

#[allow(dead_code)]
impl ArchPCBInfo {
    /// 创建一个新的ArchPCBInfo
    ///
    /// ## 参数
    ///
    /// - `kstack`：内核栈的引用
    ///
    /// ## 返回值
    ///
    /// 返回一个新的ArchPCBInfo
    pub fn new(kstack: &KernelStack) -> Self {
        Self {
            ra: 0,
            ksp: kstack.stack_max_address().data(),
            s0: 0,
            s1: 0,
            s2: 0,
            s3: 0,
            s4: 0,
            s5: 0,
            s6: 0,
            s7: 0,
            s8: 0,
            s9: 0,
            s10: 0,
            s11: 0,
            fp_state: FpDExtState::new(),
            local_context: LocalContext::new(ProcessorId::new(0)),
        }
    }
    // ### 从另一个ArchPCBInfo处clone,但是保留部分字段不变
    pub fn clone_from(&mut self, from: &Self) {
        *self = from.clone();
    }

    pub fn set_stack(&mut self, stack: VirtAddr) {
        self.ksp = stack.data();
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct FpDExtState {
    f: [u64; 32],
    fcsr: u32,
}

impl FpDExtState {
    /// 创建一个新的FpState
    const fn new() -> Self {
        Self {
            f: [0; 32],
            fcsr: 0,
        }
    }

    fn save(&mut self, regs: &mut TrapFrame) {
        if regs.status.fs() == riscv::register::sstatus::FS::Dirty {
            self.do_save();
            self.do_clean(regs);
        }
    }

    fn restore(&mut self, regs: &mut TrapFrame) {
        if regs.status.fs() != riscv::register::sstatus::FS::Off {
            self.do_restore();
            self.do_clean(regs);
        }
    }

    fn do_clean(&mut self, regs: &mut TrapFrame) {
        regs.status.update_fs(riscv::register::sstatus::FS::Clean);
    }

    fn do_save(&mut self) {
        compiler_fence(Ordering::SeqCst);
        unsafe {
            riscv::register::sstatus::set_fs(riscv::register::sstatus::FS::Initial);
            asm!("frcsr {0}", lateout(reg) self.fcsr);
            asm!(concat!(
                    "
                fsd f0, {0}
                fsd f1, {1}
                fsd f2, {2}
                fsd f3, {3}
                fsd f4, {4}
                fsd f5, {5}
                fsd f6, {6}
                fsd f7, {7}
                fsd f8, {8}
                fsd f9, {9}
                fsd f10, {10}
                fsd f11, {11}
                fsd f12, {12}
                fsd f13, {13}
                fsd f14, {14}
                fsd f15, {15}
                fsd f16, {16}
                fsd f17, {17}
                fsd f18, {18}
                fsd f19, {19}
                fsd f20, {20}
                fsd f21, {21}
                fsd f22, {22}
                fsd f23, {23}
                fsd f24, {24}
                fsd f25, {25}
                fsd f26, {26}
                fsd f27, {27}
                fsd f28, {28}
                fsd f29, {29}
                fsd f30, {30}
                fsd f31, {31}
                "
                ),
                lateout(reg) self.f[0],
                lateout(reg) self.f[1],
                lateout(reg) self.f[2],
                lateout(reg) self.f[3],
                lateout(reg) self.f[4],
                lateout(reg) self.f[5],
                lateout(reg) self.f[6],
                lateout(reg) self.f[7],
                lateout(reg) self.f[8],
                lateout(reg) self.f[9],
                lateout(reg) self.f[10],
                lateout(reg) self.f[11],
                lateout(reg) self.f[12],
                lateout(reg) self.f[13],
                lateout(reg) self.f[14],
                lateout(reg) self.f[15],
                lateout(reg) self.f[16],
                lateout(reg) self.f[17],
                lateout(reg) self.f[18],
                lateout(reg) self.f[19],
                lateout(reg) self.f[20],
                lateout(reg) self.f[21],
                lateout(reg) self.f[22],
                lateout(reg) self.f[23],
                lateout(reg) self.f[24],
                lateout(reg) self.f[25],
                lateout(reg) self.f[26],
                lateout(reg) self.f[27],
                lateout(reg) self.f[28],
                lateout(reg) self.f[29],
                lateout(reg) self.f[30],
                lateout(reg) self.f[31],

            );
            riscv::register::sstatus::set_fs(riscv::register::sstatus::FS::Off);
        }

        compiler_fence(Ordering::SeqCst);
    }

    fn do_restore(&mut self) {
        compiler_fence(Ordering::SeqCst);
        let fcsr = self.fcsr;
        unsafe {
            riscv::register::sstatus::set_fs(riscv::register::sstatus::FS::Initial);
            compiler_fence(Ordering::SeqCst);
            asm!(concat!(
                    "
                fld f0, {0}
                fld f1, {1}
                fld f2, {2}
                fld f3, {3}
                fld f4, {4}
                fld f5, {5}
                fld f6, {6}
                fld f7, {7}
                fld f8, {8}
                fld f9, {9}
                fld f10, {10}
                fld f11, {11}
                fld f12, {12}
                fld f13, {13}
                fld f14, {14}
                fld f15, {15}
                fld f16, {16}
                fld f17, {17}
                fld f18, {18}
                fld f19, {19}
                fld f20, {20}
                fld f21, {21}
                fld f22, {22}
                fld f23, {23}
                fld f24, {24}
                fld f25, {25}
                fld f26, {26}
                fld f27, {27}
                fld f28, {28}
                fld f29, {29}
                fld f30, {30}
                fld f31, {31}
                "
                ),
                in(reg) self.f[0],
                in(reg) self.f[1],
                in(reg) self.f[2],
                in(reg) self.f[3],
                in(reg) self.f[4],
                in(reg) self.f[5],
                in(reg) self.f[6],
                in(reg) self.f[7],
                in(reg) self.f[8],
                in(reg) self.f[9],
                in(reg) self.f[10],
                in(reg) self.f[11],
                in(reg) self.f[12],
                in(reg) self.f[13],
                in(reg) self.f[14],
                in(reg) self.f[15],
                in(reg) self.f[16],
                in(reg) self.f[17],
                in(reg) self.f[18],
                in(reg) self.f[19],
                in(reg) self.f[20],
                in(reg) self.f[21],
                in(reg) self.f[22],
                in(reg) self.f[23],
                in(reg) self.f[24],
                in(reg) self.f[25],
                in(reg) self.f[26],
                in(reg) self.f[27],
                in(reg) self.f[28],
                in(reg) self.f[29],
                in(reg) self.f[30],
                in(reg) self.f[31],
            );
            compiler_fence(Ordering::SeqCst);
            asm!("fscsr {0}", in(reg) fcsr);
            riscv::register::sstatus::set_fs(riscv::register::sstatus::FS::Off);
        }
        compiler_fence(Ordering::SeqCst);
    }
}

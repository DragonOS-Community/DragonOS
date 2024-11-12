use alloc::sync::{Arc, Weak};
use core::{
    arch::asm,
    intrinsics::unlikely,
    mem::ManuallyDrop,
    sync::atomic::{compiler_fence, Ordering},
};
use kdepends::memoffset::offset_of;
use log::error;
use riscv::register::sstatus::Sstatus;
use system_error::SystemError;

use crate::{
    arch::{
        interrupt::entry::ret_from_exception, process::kthread::kernel_thread_bootstrap_stage1,
        CurrentIrqArch,
    },
    exception::InterruptArch,
    libs::spinlock::SpinLockGuard,
    mm::VirtAddr,
    process::{
        fork::{CloneFlags, KernelCloneArgs},
        switch_finish_hook, KernelStack, ProcessControlBlock, ProcessFlags, ProcessManager,
        PROCESS_SWITCH_RESULT,
    },
    smp::cpu::ProcessorId,
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

pub unsafe fn arch_switch_to_user(trap_frame: TrapFrame) -> ! {
    // 以下代码不能发生中断
    CurrentIrqArch::interrupt_disable();

    let current_pcb = ProcessManager::current_pcb();
    let trap_frame_vaddr = VirtAddr::new(
        current_pcb.kernel_stack().stack_max_address().data() - core::mem::size_of::<TrapFrame>(),
    );

    let new_pc = VirtAddr::new(ret_from_exception as usize);

    let mut arch_guard = current_pcb.arch_info_irqsave();
    arch_guard.ksp = trap_frame_vaddr.data();

    drop(arch_guard);

    drop(current_pcb);

    compiler_fence(Ordering::SeqCst);

    // 重要！在这里之后，一定要保证上面的引用计数变量、动态申请的变量、锁的守卫都被drop了，否则可能导致内存安全问题！

    *(trap_frame_vaddr.data() as *mut TrapFrame) = trap_frame;

    compiler_fence(Ordering::SeqCst);
    ready_to_switch_to_user(trap_frame_vaddr.data(), new_pc.data());
}

#[naked]
unsafe extern "C" fn ready_to_switch_to_user(trap_frame: usize, new_pc: usize) -> ! {
    core::arch::naked_asm!(concat!(
        "
            // 设置trap frame
            mv sp, a0
            // 设置返回地址
            
            jr a1
            
            "
    ));
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
        new_arch_guard.sstatus = current_arch_guard.sstatus;

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
        // debug!(
        //     "riscv switch process: prev: {:?}, next: {:?}",
        //     prev.pid(),
        //     next.pid()
        // );
        Self::switch_process_fpu(&prev, &next);
        Self::switch_local_context(&prev, &next);

        // 切换地址空间
        let next_addr_space = next.basic().user_vm().as_ref().unwrap().clone();
        compiler_fence(Ordering::SeqCst);

        next_addr_space.read().user_mapper.utable.make_current();
        drop(next_addr_space);
        compiler_fence(Ordering::SeqCst);

        // debug!("current sum={}, prev sum={}, next_sum={}", riscv::register::sstatus::read().sum(), prev.arch_info_irqsave().sstatus.sum(), next.arch_info_irqsave().sstatus.sum());

        // 获取arch info的锁，并强制泄露其守卫（切换上下文后，在switch_finish_hook中会释放锁）
        let next_arch = SpinLockGuard::leak(next.arch_info_irqsave()) as *mut ArchPCBInfo;
        let prev_arch = SpinLockGuard::leak(prev.arch_info_irqsave()) as *mut ArchPCBInfo;

        // 恢复当前的 preempt count*2
        ProcessManager::current_pcb().preempt_enable();
        ProcessManager::current_pcb().preempt_enable();
        PROCESS_SWITCH_RESULT.as_mut().unwrap().get_mut().prev_pcb = Some(prev);
        PROCESS_SWITCH_RESULT.as_mut().unwrap().get_mut().next_pcb = Some(next);
        // debug!("riscv switch process: before to inner");
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
    core::arch::naked_asm!(concat!(
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

            addi sp , sp, -8
            sd a1, 0(sp)
            csrr a1, sstatus
            sd a1, {off_sstatus}(a0)
            ld a1, 0(sp)
            addi sp, sp, 8


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

            // save a1 temporarily
            addi sp , sp, -8
            sd a1, 0(sp)

            ld a0, {off_sstatus}(a1)
            csrw sstatus, a0
            
            // 将ra设置为标签1，并跳转到before_switch_finish_hook
            la ra, 1f
            j {before_switch_finish_hook}
            
            1:

            // restore a1
            ld a1, 0(sp)
            addi sp, sp, 8
            ld sp, {off_sp}(a1)
            ld ra, {off_ra}(a1)
            
            ret

        "
    ), 
    off_ra = const(offset_of!(ArchPCBInfo, ra)),
    off_sstatus = const(offset_of!(ArchPCBInfo, sstatus)),
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
    before_switch_finish_hook = sym before_switch_finish_hook);
}

/// 在切换上下文完成后的钩子函数(必须在这里加一个跳转函数，否则会出现relocation truncated to fit: R_RISCV_JAL错误)
unsafe extern "C" fn before_switch_finish_hook() {
    switch_finish_hook();
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
    sstatus: Sstatus,

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
        let mut sstatus = Sstatus::from(0);
        sstatus.update_sum(true);
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
            sstatus,
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
                // 为原来的a0寄存器的值在堆栈上分配空间
                addi sp, sp, -8
                sd a0, 0(sp)
                mv a0, {0}

                fsd f0, 0(a0)
                fsd f1, 8(a0)
                fsd f2, 16(a0)
                fsd f3, 24(a0)
                fsd f4, 32(a0)
                fsd f5, 40(a0)
                fsd f6, 48(a0)
                fsd f7, 56(a0)
                fsd f8, 64(a0)
                fsd f9, 72(a0)
                fsd f10, 80(a0)
                fsd f11, 88(a0)
                fsd f12, 96(a0)
                fsd f13, 104(a0)
                fsd f14, 112(a0)
                fsd f15, 120(a0)
                fsd f16, 128(a0)
                fsd f17, 136(a0)
                fsd f18, 144(a0)
                fsd f19, 152(a0)
                fsd f20, 160(a0)
                fsd f21, 168(a0)
                fsd f22, 176(a0)
                fsd f23, 184(a0)
                fsd f24, 192(a0)
                fsd f25, 200(a0)
                fsd f26, 208(a0)
                fsd f27, 216(a0)
                fsd f28, 224(a0)
                fsd f29, 232(a0)
                fsd f30, 240(a0)
                fsd f31, 248(a0)

                // 恢复a0寄存器的值
                ld a0, 0(sp)
                addi sp, sp, 8
                "
                ),
                in (reg) &self.f as *const _,
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
            // 为原来的a0寄存器的值在堆栈上分配空间
            addi sp, sp, -8
            sd a0, 0(sp)
            mv a0, {0}

            fld f0, 0(a0)
            fld f1, 8(a0)
            fld f2, 16(a0)
            fld f3, 24(a0)
            fld f4, 32(a0)
            fld f5, 40(a0)
            fld f6, 48(a0)
            fld f7, 56(a0)
            fld f8, 64(a0)
            fld f9, 72(a0)
            fld f10, 80(a0)
            fld f11, 88(a0)
            fld f12, 96(a0)
            fld f13, 104(a0)
            fld f14, 112(a0)
            fld f15, 120(a0)
            fld f16, 128(a0)
            fld f17, 136(a0)
            fld f18, 144(a0)
            fld f19, 152(a0)
            fld f20, 160(a0)
            fld f21, 168(a0)
            fld f22, 176(a0)
            fld f23, 184(a0)
            fld f24, 192(a0)
            fld f25, 200(a0)
            fld f26, 208(a0)
            fld f27, 216(a0)
            fld f28, 224(a0)
            fld f29, 232(a0)
            fld f30, 240(a0)
            fld f31, 248(a0)

            // 恢复a0寄存器的值
            ld a0, 0(sp)
            addi sp, sp, 8
            "
                ),
                in (reg) &self.f as *const _,
            );
            compiler_fence(Ordering::SeqCst);
            asm!("fscsr {0}", in(reg) fcsr);
            riscv::register::sstatus::set_fs(riscv::register::sstatus::FS::Off);
        }
        compiler_fence(Ordering::SeqCst);
    }
}

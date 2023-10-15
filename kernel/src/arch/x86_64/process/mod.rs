use core::{
    arch::asm,
    intrinsics::unlikely,
    mem::ManuallyDrop,
    sync::atomic::{compiler_fence, Ordering},
};

use alloc::{string::String, sync::Arc, vec::Vec};

use memoffset::offset_of;
use x86::{controlregs::Cr4, segmentation::SegmentSelector};

use crate::{
    arch::process::table::TSSManager,
    exception::InterruptArch,
    kwarn,
    libs::spinlock::SpinLockGuard,
    mm::{
        percpu::{PerCpu, PerCpuVar},
        VirtAddr,
    },
    process::{
        fork::CloneFlags, KernelStack, ProcessControlBlock, ProcessFlags, ProcessManager,
        SwitchResult, SWITCH_RESULT,
    },
    syscall::{Syscall, SystemError},
};

use self::{
    kthread::kernel_thread_bootstrap_stage1,
    table::{switch_fs_and_gs, KERNEL_DS, USER_DS},
};

use super::{fpu::FpState, interrupt::TrapFrame, CurrentIrqArch};

mod c_adapter;
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
#[derive(Debug, Clone)]
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
    fs: u16,
    gs: u16,

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
    pub fn new(kstack: Option<&KernelStack>) -> Self {
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
            fs: KERNEL_DS.bits(),
            gs: KERNEL_DS.bits(),
            fp_state: None,
        };

        if kstack.is_some() {
            let kstack = kstack.unwrap();
            r.rsp = kstack.stack_max_address().data();
            r.rbp = kstack.stack_max_address().data();
        }

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
    pub fn fp_state(&self) -> Option<FpState> {
        self.fp_state.clone()
    }

    // 清空浮点寄存器
    pub fn clear_fp_state(&mut self) {
        if unlikely(self.fp_state.is_none()) {
            kwarn!("fp_state is none");
            return;
        }

        self.fp_state.as_mut().unwrap().clear();
    }
    pub unsafe fn save_fsbase(&mut self) {
        if x86::controlregs::cr4().contains(Cr4::CR4_ENABLE_FSGSBASE) {
            self.fsbase = x86::current::segmentation::rdfsbase() as usize;
        } else {
            self.fsbase = 0;
        }
    }

    pub unsafe fn save_gsbase(&mut self) {
        if x86::controlregs::cr4().contains(Cr4::CR4_ENABLE_FSGSBASE) {
            self.gsbase = x86::current::segmentation::rdgsbase() as usize;
        } else {
            self.gsbase = 0;
        }
    }

    pub unsafe fn restore_fsbase(&mut self) {
        if x86::controlregs::cr4().contains(Cr4::CR4_ENABLE_FSGSBASE) {
            x86::current::segmentation::wrfsbase(self.fsbase as u64);
        }
    }

    pub unsafe fn restore_gsbase(&mut self) {
        if x86::controlregs::cr4().contains(Cr4::CR4_ENABLE_FSGSBASE) {
            x86::current::segmentation::wrgsbase(self.gsbase as u64);
        }
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
            panic!("current_pcb is null");
        }
        unsafe {
            // 为了防止内核栈的pcb指针被释放，这里需要将其包装一下，使得Arc的drop不会被调用
            let arc_wrapper: ManuallyDrop<Arc<ProcessControlBlock>> =
                ManuallyDrop::new(Arc::from_raw(*p));

            let new_arc: Arc<ProcessControlBlock> = Arc::clone(&arc_wrapper);
            return new_arc;
        }
    }
}

impl ProcessManager {
    pub fn arch_init() {
        {
            // 初始化进程切换结果 per cpu变量
            let mut switch_res_vec: Vec<SwitchResult> = Vec::new();
            for _ in 0..PerCpu::MAX_CPU_NUM {
                switch_res_vec.push(SwitchResult::new());
            }
            unsafe {
                SWITCH_RESULT = Some(PerCpuVar::new(switch_res_vec).unwrap());
            }
        }
    }
    /// fork的过程中复制线程
    ///
    /// 由于这个过程与具体的架构相关，所以放在这里
    pub fn copy_thread(
        _clone_flags: &CloneFlags,
        current_pcb: &Arc<ProcessControlBlock>,
        new_pcb: &Arc<ProcessControlBlock>,
        current_trapframe: &TrapFrame,
    ) -> Result<(), SystemError> {
        let mut child_trapframe = current_trapframe.clone();

        // 子进程的返回值为0
        child_trapframe.set_return_value(0);

        // 设置子进程的栈基址（开始执行中断返回流程时的栈基址）
        let mut new_arch_guard = new_pcb.arch_info();
        let kernel_stack_guard = new_pcb.kernel_stack();

        // 设置子进程在内核态开始执行时的rsp、rbp
        new_arch_guard.set_stack_base(kernel_stack_guard.stack_max_address());

        let trap_frame_vaddr: VirtAddr =
            kernel_stack_guard.stack_max_address() - core::mem::size_of::<TrapFrame>();
        new_arch_guard.set_stack(trap_frame_vaddr);

        // 拷贝栈帧
        unsafe {
            let trap_frame_ptr = trap_frame_vaddr.data() as *mut TrapFrame;
            *trap_frame_ptr = child_trapframe;
        }

        let current_arch_guard = current_pcb.arch_info_irqsave();
        new_arch_guard.fsbase = current_arch_guard.fsbase;
        new_arch_guard.gsbase = current_arch_guard.gsbase;
        new_arch_guard.fs = current_arch_guard.fs;
        new_arch_guard.gs = current_arch_guard.gs;
        new_arch_guard.fp_state = current_arch_guard.fp_state.clone();

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

        return Ok(());
    }

    /// 切换进程
    ///
    /// ## 参数
    ///
    /// - `prev`：上一个进程的pcb
    /// - `next`：下一个进程的pcb
    pub unsafe fn switch_process(prev: Arc<ProcessControlBlock>, next: Arc<ProcessControlBlock>) {
        assert!(CurrentIrqArch::is_irq_enabled() == false);

        // 保存浮点寄存器
        prev.arch_info().save_fp_state();
        // 切换浮点寄存器
        next.arch_info().restore_fp_state();

        // 切换fsbase
        prev.arch_info().save_fsbase();
        next.arch_info().restore_fsbase();

        // 切换gsbase
        prev.arch_info().save_gsbase();
        next.arch_info().restore_gsbase();

        // 切换地址空间
        let next_addr_space = next.basic().user_vm().as_ref().unwrap().clone();
        compiler_fence(Ordering::SeqCst);

        next_addr_space.read().user_mapper.utable.make_current();
        compiler_fence(Ordering::SeqCst);
        // 切换内核栈

        // 获取arch info的锁，并强制泄露其守卫（切换上下文后，在switch_finish_hook中会释放锁）
        let next_arch = SpinLockGuard::leak(next.arch_info());
        let prev_arch = SpinLockGuard::leak(prev.arch_info());

        prev_arch.rip = switch_back as usize;

        // 恢复当前的 preempt count*2
        ProcessManager::current_pcb().preempt_enable();
        ProcessManager::current_pcb().preempt_enable();
        SWITCH_RESULT.as_mut().unwrap().get_mut().prev_pcb = Some(prev.clone());
        SWITCH_RESULT.as_mut().unwrap().get_mut().next_pcb = Some(next.clone());

        // 切换tss
        TSSManager::current_tss().set_rsp(
            x86::Ring::Ring0,
            next.kernel_stack().stack_max_address().data() as u64,
        );
        // kdebug!("switch tss ok");

        compiler_fence(Ordering::SeqCst);
        // 正式切换上下文
        switch_to_inner(prev_arch, next_arch);
    }
}

/// 保存上下文，然后切换进程，接着jmp到`switch_finish_hook`钩子函数
#[naked]
unsafe extern "sysv64" fn switch_to_inner(prev: &mut ArchPCBInfo, next: &mut ArchPCBInfo) {
    asm!(
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

        push rbp
        push rax

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
        options(noreturn),
    );
}

/// 从`switch_to_inner`返回后，执行这个函数
///
/// 也就是说，当进程再次被调度时，会从这里开始执行
#[inline(never)]
unsafe extern "sysv64" fn switch_back() {
    asm!(concat!(
        "
        pop rax
        pop rbp
        "
    ))
}

pub unsafe fn arch_switch_to_user(path: String, argv: Vec<String>, envp: Vec<String>) -> ! {
    // 以下代码不能发生中断
    CurrentIrqArch::interrupt_disable();

    let current_pcb = ProcessManager::current_pcb();
    let trap_frame_vaddr = VirtAddr::new(
        current_pcb.kernel_stack().stack_max_address().data() - core::mem::size_of::<TrapFrame>(),
    );
    // kdebug!("trap_frame_vaddr: {:?}", trap_frame_vaddr);
    let new_rip = VirtAddr::new(ret_from_intr as usize);

    assert!(
        (x86::current::registers::rsp() as usize) < trap_frame_vaddr.data(),
        "arch_switch_to_user(): current_rsp >= fake trap 
        frame vaddr, this may cause some illegal access to memory! 
        rsp: {:#x}, trap_frame_vaddr: {:#x}",
        x86::current::registers::rsp() as usize,
        trap_frame_vaddr.data()
    );

    let mut arch_guard = current_pcb.arch_info_irqsave();
    arch_guard.rsp = trap_frame_vaddr.data();

    arch_guard.fs = USER_DS.bits();
    arch_guard.gs = USER_DS.bits();

    switch_fs_and_gs(
        SegmentSelector::from_bits_truncate(arch_guard.fs),
        SegmentSelector::from_bits_truncate(arch_guard.gs),
    );
    arch_guard.rip = new_rip.data();

    drop(arch_guard);

    // 删除kthread的标志
    current_pcb.flags().remove(ProcessFlags::KTHREAD);
    current_pcb.worker_private().take();

    let mut trap_frame = TrapFrame::new();

    compiler_fence(Ordering::SeqCst);
    Syscall::do_execve(path, argv, envp, &mut trap_frame).unwrap_or_else(|e| {
        panic!(
            "arch_switch_to_user(): pid: {pid:?}, Failed to execve: , error: {e:?}",
            pid = current_pcb.pid(),
            e = e
        );
    });
    compiler_fence(Ordering::SeqCst);

    // 重要！在这里之后，一定要保证上面的引用计数变量、动态申请的变量、锁的守卫都被drop了，否则可能导致内存安全问题！

    drop(current_pcb);

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
    asm!(
        "mov rsp, {trapframe_vaddr}",
        "push {new_rip}",
        "ret",
        trapframe_vaddr = in(reg) trapframe_vaddr,
        new_rip = in(reg) new_rip
    );
    unreachable!()
}

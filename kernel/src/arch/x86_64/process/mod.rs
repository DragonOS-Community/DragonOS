use core::{arch::asm, intrinsics::unlikely, mem::ManuallyDrop};

use alloc::{sync::Arc, vec::Vec};

use memoffset::offset_of;

use crate::{
    libs::spinlock::SpinLockGuard,
    mm::{
        percpu::{PerCpu, PerCpuVar},
        VirtAddr,
    },
    process::{
        fork::CloneFlags, KernelStack, ProcessControlBlock, ProcessFlags, ProcessManager,
        SwitchResult, SWITCH_RESULT,
    },
    syscall::SystemError,
};

use super::{fpu::FpState, interrupt::TrapFrame};

pub mod kthread;
pub mod syscall;
mod table;

extern "C" {
    /// 内核线程引导函数
    fn kernel_thread_func();
    /// 从中断返回
    fn ret_from_intr();
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

    /// 浮点寄存器的状态
    fp_state: Option<FpState>,
}

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
        if self.fp_state.is_none() {
            panic!("fp_state is none");
        }

        self.fp_state.as_mut().unwrap().restore();
    }

    pub unsafe fn save_fsbase(&mut self) {
        self.fsbase = x86::current::segmentation::rdfsbase() as usize;
    }

    pub unsafe fn save_gsbase(&mut self) {
        self.gsbase = x86::current::segmentation::rdgsbase() as usize;
    }

    pub fn fsbase(&self) -> usize {
        self.fsbase
    }

    pub fn gsbase(&self) -> usize {
        self.gsbase
    }
}

impl ProcessControlBlock {
    /// 获取当前进程的pcb
    pub fn arch_current_pcb() -> Arc<Self> {
        // 获取栈指针
        let ptr = VirtAddr::new(x86::current::registers::rsp() as usize);
        let stack_base = VirtAddr::new(ptr.data() & (!(KernelStack::ALIGN - 1)));
        // 从内核栈的最低地址处取出pcb的地址
        let p = stack_base.data() as *const ProcessControlBlock;
        if unlikely(p.is_null()) {
            panic!("current_pcb is null");
        }
        unsafe {
            // 为了防止内核栈的pcb指针被释放，这里需要将其包装一下，使得Arc的drop不会被调用
            let arc_wrapper: ManuallyDrop<Arc<ProcessControlBlock>> =
                ManuallyDrop::new(Arc::from_raw(p));

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
        clone_flags: &CloneFlags,
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
        new_arch_guard.set_stack_base(kernel_stack_guard.stack_max_address());

        let trap_frame_vaddr: VirtAddr =
            kernel_stack_guard.stack_max_address() - core::mem::size_of::<TrapFrame>();
        new_arch_guard.set_stack(trap_frame_vaddr);

        // 拷贝栈帧
        unsafe {
            let trap_frame_ptr = trap_frame_vaddr.data() as *mut TrapFrame;
            *trap_frame_ptr = child_trapframe;
        }

        new_arch_guard.fsbase = current_pcb.arch_info().fsbase;
        new_arch_guard.gsbase = current_pcb.arch_info().gsbase;

        // 拷贝浮点寄存器的状态
        if let Some(fp_state) = current_pcb.arch_info().fp_state.as_ref() {
            new_arch_guard.fp_state = Some(*fp_state);
        }

        // 设置返回地址（子进程开始执行的指令地址）
        if new_pcb.flags().contains(ProcessFlags::KTHREAD) {
            new_arch_guard.rip = kernel_thread_func as usize;
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
        // 保存浮点寄存器
        prev.arch_info().save_fp_state();
        // 切换浮点寄存器
        next.arch_info().restore_fp_state();

        // 切换fsbase
        prev.arch_info().save_fsbase();
        x86::msr::wrmsr(x86::msr::IA32_FS_BASE, next.arch_info().fsbase as u64);

        // 切换gsbase
        prev.arch_info().save_gsbase();
        x86::msr::wrmsr(x86::msr::IA32_KERNEL_GSBASE, next.arch_info().gsbase as u64);

        // 切换地址空间
        let next_addr_space = next.basic().user_vm().as_ref().unwrap().clone();
        next_addr_space.read().user_mapper.utable.make_current();

        // 切换内核栈

        // 获取arch info的锁，并强制泄露其守卫（切换上下文后，在switch_finish_hook中会释放锁）
        let next_arch = SpinLockGuard::leak(next.arch_info());
        let prev_arch = SpinLockGuard::leak(prev.arch_info());
        // 恢复当前的 preempt count*2
        ProcessManager::current_pcb().preempt_enable();
        ProcessManager::current_pcb().preempt_enable();
        SWITCH_RESULT.as_mut().unwrap().get_mut().next_pcb = Some(next.clone());
        SWITCH_RESULT.as_mut().unwrap().get_mut().prev_pcb = Some(next.clone());

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

        mov [rdi + {off_rbp}], rbp
        mov rbp, [rsi + {off_rbp}]

        mov [rdi + {off_rsp}], rsp
        mov rsp, [rsi + {off_rsp}]

        // push RFLAGS (can only be modified via stack)
        pushfq
        // pop RFLAGS into `self.rflags`
        pop QWORD PTR [rdi + {off_rflags}]

        // push `next.rflags`
        push QWORD PTR [rsi + {off_rflags}]
        // pop into RFLAGS
        popfq

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

        switch_hook = sym crate::process::switch_finish_hook,
        options(noreturn),
    );
}

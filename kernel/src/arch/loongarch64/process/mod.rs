use alloc::sync::Arc;
use system_error::SystemError;

use crate::{
    arch::CurrentIrqArch,
    exception::InterruptArch,
    mm::VirtAddr,
    process::{fork::KernelCloneArgs, KernelStack, ProcessControlBlock, ProcessManager},
};

use super::interrupt::TrapFrame;

pub mod idle;
pub mod kthread;
pub mod syscall;

#[repr(align(32768))]
pub union InitProcUnion {
    /// 用于存放idle进程的内核栈
    idle_stack: [u8; 32768],
}

#[link_section = ".data.init_proc_union"]
#[no_mangle]
pub(super) static BSP_IDLE_STACK_SPACE: InitProcUnion = InitProcUnion {
    idle_stack: [0; 32768],
};

pub unsafe fn arch_switch_to_user(trap_frame: TrapFrame) -> ! {
    // 以下代码不能发生中断
    CurrentIrqArch::interrupt_disable();

    todo!("la64: arch_switch_to_user")
}

/// PCB中与架构相关的信息
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
#[repr(C)]
pub struct ArchPCBInfo {}

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
        todo!("la64: ArchPCBInfo::new")
    }
}

impl ProcessControlBlock {
    /// 获取当前进程的pcb
    pub fn arch_current_pcb() -> Arc<Self> {
        todo!("la64: arch_current_pcb")
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
        todo!("la64: copy_thread")
    }

    /// 切换进程
    ///
    /// ## 参数
    ///
    /// - `prev`：上一个进程的pcb
    /// - `next`：下一个进程的pcb
    pub unsafe fn switch_process(prev: Arc<ProcessControlBlock>, next: Arc<ProcessControlBlock>) {
        assert!(!CurrentIrqArch::is_irq_enabled());
        todo!("la64: switch_process");
    }
}

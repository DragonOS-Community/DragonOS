use alloc::{string::String, sync::Arc, vec::Vec};

use crate::{
    process::{fork::KernelCloneArgs, KernelStack, ProcessControlBlock, ProcessManager},
    syscall::SystemError,
};

use super::interrupt::TrapFrame;

pub mod kthread;
pub mod syscall;

pub unsafe fn arch_switch_to_user(path: String, argv: Vec<String>, envp: Vec<String>) -> ! {
    unimplemented!("RiscV64 arch_switch_to_user")
}

impl ProcessManager {
    pub fn arch_init() {
        unimplemented!("ProcessManager::arch_init")
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
        unimplemented!("ProcessManager::copy_thread")
    }

    /// 切换进程
    ///
    /// ## 参数
    ///
    /// - `prev`：上一个进程的pcb
    /// - `next`：下一个进程的pcb
    pub unsafe fn switch_process(prev: Arc<ProcessControlBlock>, next: Arc<ProcessControlBlock>) {
        unimplemented!("ProcessManager::switch_process")
    }
}

impl ProcessControlBlock {
    /// 获取当前进程的pcb
    pub fn arch_current_pcb() -> Arc<Self> {
        unimplemented!("ProcessControlBlock::arch_current_pcb")
    }
}

/// PCB中与架构相关的信息
#[derive(Debug)]
#[allow(dead_code)]
pub struct ArchPCBInfo {
    // todo: add arch related fields
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
        unimplemented!("ArchPCBInfo::new")
    }
    // ### 从另一个ArchPCBInfo处clone,但是保留部分字段不变
    pub fn clone_from(&mut self, from: &Self) {
        unimplemented!("ArchPCBInfo::clone_from")
    }
}

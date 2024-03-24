use core::{arch::asm, mem::ManuallyDrop};

use alloc::{
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

use crate::{
    kerror,
    mm::VirtAddr,
    process::{fork::KernelCloneArgs, KernelStack, ProcessControlBlock, ProcessManager},
};

use super::interrupt::TrapFrame;

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
        unimplemented!("ProcessManager::copy_thread")
    }

    /// 切换进程
    ///
    /// ## 参数
    ///
    /// - `prev`：上一个进程的pcb
    /// - `next`：下一个进程的pcb
    pub unsafe fn switch_process(prev: Arc<ProcessControlBlock>, next: Arc<ProcessControlBlock>) {
        // todo: https://code.dragonos.org.cn/xref/linux-6.6.21/arch/riscv/include/asm/switch_to.h#76
        unimplemented!("ProcessManager::switch_process")
    }
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
        Self {}
    }
    // ### 从另一个ArchPCBInfo处clone,但是保留部分字段不变
    pub fn clone_from(&mut self, from: &Self) {
        unimplemented!("ArchPCBInfo::clone_from")
    }
}

use core::{intrinsics::unlikely, mem::ManuallyDrop};

use alloc::sync::Arc;

use crate::{
    mm::VirtAddr,
    process::{KernelStack, ProcessControlBlock},
};

/// PCB中与架构相关的信息
#[derive(Debug)]
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
}

impl ArchPCBInfo {
    pub fn new() -> Self {
        Self {
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
        }
    }

    pub fn set_stack(&mut self, stack: usize) {
        self.rsp = stack;
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

use core::{
    intrinsics::unlikely,
    sync::atomic::{AtomicBool, Ordering},
};

use alloc::{sync::Arc, vec::Vec};

use crate::{
    mm::{percpu::PerCpu, VirtAddr, INITIAL_PROCESS_ADDRESS_SPACE},
    process::KernelStack,
    smp::{core::smp_get_processor_id, cpu::ProcessorId},
};

use super::{ProcessControlBlock, ProcessManager};

static mut __IDLE_PCB: Option<Vec<Arc<ProcessControlBlock>>> = None;

impl ProcessManager {
    /// 初始化每个核的idle进程
    pub fn init_idle() {
        static INIT_IDLE: AtomicBool = AtomicBool::new(false);
        if INIT_IDLE
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            panic!("Idle process already initialized");
        }

        assert!(
            smp_get_processor_id() == ProcessorId::new(0),
            "Idle process must be initialized on the first processor"
        );
        let mut v: Vec<Arc<ProcessControlBlock>> = Vec::with_capacity(PerCpu::MAX_CPU_NUM as usize);

        for i in 0..PerCpu::MAX_CPU_NUM {
            let kstack = if unlikely(i == 0) {
                let stack_ptr =
                    VirtAddr::new(Self::stack_ptr().data() & (!(KernelStack::ALIGN - 1)));
                // 初始化bsp的idle进程
                let mut ks = unsafe { KernelStack::from_existed(stack_ptr) }
                    .expect("Failed to create kernel stack struct for BSP.");
                unsafe { ks.clear_pcb(true) };
                ks
            } else {
                KernelStack::new().unwrap_or_else(|e| {
                    panic!("Failed to create kernel stack struct for AP {}: {:?}", i, e)
                })
            };

            let idle_pcb = ProcessControlBlock::new_idle(i as u32, kstack);

            assert!(idle_pcb.basic().user_vm().is_none());
            unsafe {
                idle_pcb
                    .basic_mut()
                    .set_user_vm(Some(INITIAL_PROCESS_ADDRESS_SPACE()))
            };

            assert!(idle_pcb.sched_info().on_cpu().is_none());
            idle_pcb.sched_info().set_on_cpu(Some(ProcessorId::new(i)));
            v.push(idle_pcb);
        }

        unsafe {
            __IDLE_PCB = Some(v);
        }
    }

    /// 获取当前的栈指针
    ///
    /// 请注意，该函数只是于辅助bsp核心的idle进程初始化
    fn stack_ptr() -> VirtAddr {
        #[cfg(target_arch = "x86_64")]
        return VirtAddr::new(x86::current::registers::rsp() as usize);

        #[cfg(target_arch = "riscv64")]
        unimplemented!("stack_ptr() is not implemented on RISC-V")
    }

    /// 获取idle进程数组的引用
    pub fn idle_pcb() -> &'static Vec<Arc<ProcessControlBlock>> {
        unsafe { __IDLE_PCB.as_ref().unwrap() }
    }
}

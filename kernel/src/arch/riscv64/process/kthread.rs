use alloc::sync::Arc;
use riscv::register::sstatus::SPP;
use system_error::SystemError;

use crate::{
    arch::interrupt::TrapFrame,
    process::{
        fork::CloneFlags,
        kthread::{KernelThreadCreateInfo, KernelThreadMechanism},
        Pid, ProcessManager,
    },
};

impl KernelThreadMechanism {
    /// 伪造trapframe，创建内核线程
    ///
    /// ## 返回值
    ///
    /// 返回创建的内核线程的pid
    #[inline(never)]
    pub fn __inner_create(
        info: &Arc<KernelThreadCreateInfo>,
        clone_flags: CloneFlags,
    ) -> Result<Pid, SystemError> {
        // WARNING: If create failed, we must drop the info manually or it will cause memory leak. (refcount will not decrease when create failed)
        let create_info: *const KernelThreadCreateInfo =
            KernelThreadCreateInfo::generate_unsafe_arc_ptr(info.clone());

        let mut frame = TrapFrame::new();
        frame.a0 = create_info as usize;

        // 使能中断
        frame.status.update_sie(true);
        frame.status.update_spp(SPP::Supervisor);

        frame.ra = kernel_thread_bootstrap_stage1 as usize;

        // fork失败的话，子线程不会执行。否则将导致内存安全问题。
        let pid = ProcessManager::fork(&frame, clone_flags).map_err(|e| {
            unsafe { KernelThreadCreateInfo::parse_unsafe_arc_ptr(create_info) };
            e
        })?;

        ProcessManager::find(pid)
            .unwrap()
            .set_name(info.name().clone());

        return Ok(pid);
    }
}

/// 内核线程引导函数的第一阶段
///
/// 当内核线程开始执行时，会先执行这个函数，这个函数会将伪造的trapframe中的数据弹出，然后跳转到第二阶段
///
/// 跳转之后，指向Box<KernelThreadClosure>的指针将传入到stage2的函数
// #[naked]
// pub(super) unsafe extern "C" fn kernel_thread_bootstrap_stage1() {
//     todo!()
// }
pub(super) unsafe extern "C" fn kernel_thread_bootstrap_stage1() {
    // 这个函数要是naked的，只是因为现在还没有实现，而naked func不能打`unimplemented!()`
    // 所以先写成了普通函数
    unimplemented!("kernel_thread_bootstrap_stage1")
}

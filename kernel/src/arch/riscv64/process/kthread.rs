use alloc::sync::Arc;
use system_error::SystemError;

use crate::process::{
    fork::CloneFlags,
    kthread::{KernelThreadCreateInfo, KernelThreadMechanism},
    Pid,
};

impl KernelThreadMechanism {
    /// 伪造trapframe，创建内核线程
    ///
    /// ## 返回值
    ///
    /// 返回创建的内核线程的pid
    pub fn __inner_create(
        info: &Arc<KernelThreadCreateInfo>,
        clone_flags: CloneFlags,
    ) -> Result<Pid, SystemError> {
        unimplemented!("KernelThreadMechanism::__inner_create")
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

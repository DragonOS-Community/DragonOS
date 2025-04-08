use system_error::SystemError;

use alloc::sync::Arc;

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
        // WARNING: If create failed, we must drop the info manually or it will cause memory leak. (refcount will not decrease when create failed)
        let create_info: *const KernelThreadCreateInfo =
            KernelThreadCreateInfo::generate_unsafe_arc_ptr(info.clone());

        todo!("la64:__inner_create()")
    }
}

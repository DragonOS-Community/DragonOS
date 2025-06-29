use alloc::sync::Weak;

use alloc::sync::Arc;
use system_error::SystemError;

use crate::process::fork::CloneFlags;

pub struct PidNamespace {
    self_ref: Weak<PidNamespace>,
}

impl PidNamespace {
    /// 创建root PID namespace
    pub fn new_root() -> Arc<Self> {
        Arc::new_cyclic(|self_ref| Self {
            self_ref: self_ref.clone(),
        })
    }

    /// https://code.dragonos.org.cn/xref/linux-6.6.21/kernel/pid_namespace.c#145
    pub(super) fn copy_pid_ns(&self, clone_flags: &CloneFlags) -> Result<Arc<Self>, SystemError> {
        if !clone_flags.contains(CloneFlags::CLONE_NEWPID) {
            return Ok(self.self_ref.upgrade().unwrap());
        }

        todo!("Implement new PID namespace creation logic");
    }
}

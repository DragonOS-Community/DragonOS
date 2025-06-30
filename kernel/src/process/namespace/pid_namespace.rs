use alloc::sync::Weak;

use alloc::sync::Arc;
use hashbrown::HashMap;
use ida::IdAllocator;
use system_error::SystemError;

use crate::libs::spinlock::SpinLock;
use crate::process::fork::CloneFlags;
use crate::process::ProcessControlBlock;
use crate::process::RawPid;

use super::nsproxy::NsCommon;

pub struct PidNamespace {
    self_ref: Weak<PidNamespace>,
    /// PID namespace的层级（root = 0）
    pub level: u32,
    /// 父namespace的弱引用
    parent: Option<Weak<PidNamespace>>,

    /// init进程引用
    child_reaper: Option<Weak<ProcessControlBlock>>,

    inner: SpinLock<InnerPidNamespace>,
}

pub struct InnerPidNamespace {
    pub ns_common: NsCommon,
    ida: IdAllocator,
    /// PID到进程的映射表
    pid_map: HashMap<RawPid, Weak<ProcessControlBlock>>,
}

impl PidNamespace {
    /// 创建root PID namespace
    pub fn new_root() -> Arc<Self> {
        Arc::new_cyclic(|self_ref| Self {
            self_ref: self_ref.clone(),
            level: 0,
            parent: None,
            child_reaper: None,
            inner: SpinLock::new(InnerPidNamespace {
                ns_common: NsCommon {
                    stashed: core::sync::atomic::AtomicIsize::new(0),
                },
                ida: IdAllocator::new(1, usize::MAX).unwrap(),
                pid_map: HashMap::new(),
            }),
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

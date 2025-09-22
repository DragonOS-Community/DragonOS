use alloc::sync::{Arc, Weak};

use crate::ipc::shm::ShmManager;
use crate::libs::spinlock::SpinLock;
use crate::process::namespace::{
    nsproxy::NsCommon, user_namespace::UserNamespace, NamespaceOps, NamespaceType,
};
use crate::process::ProcessManager;

// 根 IPC 命名空间
lazy_static::lazy_static! {
    pub static ref INIT_IPC_NAMESPACE: Arc<IpcNamespace> = IpcNamespace::new_root();
}

/// DragonOS 的 IPC 命名空间
pub struct IpcNamespace {
    ns_common: NsCommon,
    self_ref: Weak<IpcNamespace>,
    /// 关联的 user namespace (权限判断使用)
    pub user_ns: Arc<UserNamespace>,

    /// SysV SHM 管理器（阶段一：仅支持 per-ns shm）
    pub shm: SpinLock<ShmManager>,
}

impl NamespaceOps for IpcNamespace {
    fn ns_common(&self) -> &NsCommon {
        &self.ns_common
    }
}

impl IpcNamespace {
    fn new_root() -> Arc<Self> {
        Arc::new_cyclic(|weak_self| Self {
            ns_common: NsCommon::new(0, NamespaceType::Ipc),
            self_ref: weak_self.clone(),
            user_ns: crate::process::namespace::user_namespace::INIT_USER_NAMESPACE.clone(),
            shm: SpinLock::new(ShmManager::new()),
        })
    }

    /// 复制/创建 IPC 命名空间
    pub fn copy_ipc_ns(
        &self,
        clone_flags: &crate::process::fork::CloneFlags,
        user_ns: Arc<UserNamespace>,
    ) -> Arc<IpcNamespace> {
        use crate::process::fork::CloneFlags;
        if !clone_flags.contains(CloneFlags::CLONE_NEWIPC) {
            return self.self_ref.upgrade().unwrap();
        }
        // 创建新的 IPC 命名空间，SHM 空间独立
        Arc::new_cyclic(|weak_self| IpcNamespace {
            ns_common: NsCommon::new(self.ns_common.level + 1, NamespaceType::Ipc),
            self_ref: weak_self.clone(),
            user_ns,
            shm: SpinLock::new(ShmManager::new()),
        })
    }
}

impl ProcessManager {
    pub fn current_ipcns() -> Arc<IpcNamespace> {
        if Self::initialized() {
            ProcessManager::current_pcb().nsproxy.read().ipc_ns.clone()
        } else {
            INIT_IPC_NAMESPACE.clone()
        }
    }
}

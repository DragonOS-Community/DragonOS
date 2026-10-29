use alloc::sync::{Arc, Weak};

use crate::ipc::sem::SemManager;
use crate::ipc::shm::ShmManager;
use crate::libs::spinlock::SpinLock;
use crate::process::namespace::{
    nsproxy::NsCommon, user_namespace::UserNamespace, NamespaceOps, NamespaceType,
};
use crate::process::ProcessManager;

// Root IPC namespace
lazy_static::lazy_static! {
    pub static ref INIT_IPC_NAMESPACE: Arc<IpcNamespace> = IpcNamespace::new_root();
}

/// DragonOS IPC namespace
pub struct IpcNamespace {
    ns_common: NsCommon,
    self_ref: Weak<IpcNamespace>,
    /// Associated user namespace (used for permission checks)
    pub user_ns: Arc<UserNamespace>,

    /// SysV SHM manager (phase one: per-namespace SHM only)
    pub shm: SpinLock<ShmManager>,
    /// SysV semaphore manager
    pub sem: SpinLock<SemManager>,
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
            sem: SpinLock::new(SemManager::new()),
        })
    }

    /// Copy or create an IPC namespace
    pub fn copy_ipc_ns(
        &self,
        clone_flags: &crate::process::fork::CloneFlags,
        user_ns: Arc<UserNamespace>,
    ) -> Arc<IpcNamespace> {
        use crate::process::fork::CloneFlags;
        if !clone_flags.contains(CloneFlags::CLONE_NEWIPC) {
            return self.self_ref.upgrade().unwrap();
        }
        // Create an independent IPC namespace with separate SHM and semaphore spaces.
        Arc::new_cyclic(|weak_self| IpcNamespace {
            ns_common: NsCommon::new(self.ns_common.level + 1, NamespaceType::Ipc),
            self_ref: weak_self.clone(),
            user_ns,
            shm: SpinLock::new(ShmManager::new()),
            sem: SpinLock::new(SemManager::new()),
        })
    }
}

impl ProcessManager {
    pub fn current_ipcns() -> Arc<IpcNamespace> {
        if Self::initialized() {
            ProcessManager::current_pcb().nsproxy().ipc_ns.clone()
        } else {
            INIT_IPC_NAMESPACE.clone()
        }
    }
}

use alloc::sync::Arc;
use mnt_namespace::{FsStruct, MntNamespace};
use pid_namespace::PidNamespace;
use system_error::SystemError;
use user_namespace::UserNamespace;

use crate::{
    libs::spinlock::SpinLock,
    process::{fork::CloneFlags, ProcessControlBlock},
};

pub mod mnt_namespace;
pub mod namespace;
pub mod pid_namespace;
pub mod syscall;
pub mod ucount;
pub mod user_namespace;

/// 管理 namespace,包含了所有namespace的信息
#[derive(Clone)]
pub struct NsSet {
    flags: u64,
    nsproxy: NsProxy,
    pub fs: Arc<SpinLock<FsStruct>>,
}
#[derive(Debug, Clone)]
pub struct NsProxy {
    pub pid_namespace: Arc<PidNamespace>,
    pub mnt_namespace: Arc<MntNamespace>,
}
impl Default for NsProxy {
    fn default() -> Self {
        Self {
            pid_namespace: Arc::new(PidNamespace::default()),
            mnt_namespace: Arc::new(MntNamespace::default()),
        }
    }
}

impl NsProxy {
    pub fn set_pid_namespace(&mut self, new_pid_ns: Arc<PidNamespace>) {
        self.pid_namespace = new_pid_ns;
    }

    pub fn set_mnt_namespace(&mut self, new_mnt_ns: Arc<MntNamespace>) {
        self.mnt_namespace = new_mnt_ns;
    }
}

pub fn create_new_namespaces(
    clone_flags: u64,
    pcb: &Arc<ProcessControlBlock>,
    user_ns: Arc<UserNamespace>,
) -> Result<NsProxy, SystemError> {
    let mut nsproxy = NsProxy::default();
    // pid_namespace
    let new_pid_ns = if (clone_flags & CloneFlags::CLONE_NEWPID.bits()) != 0 {
        Arc::new(PidNamespace::default().create_pid_namespace(
            pcb.get_nsproxy().read().pid_namespace.clone(),
            user_ns.clone(),
        )?)
    } else {
        pcb.get_nsproxy().read().pid_namespace.clone()
    };
    nsproxy.set_pid_namespace(new_pid_ns);

    // mnt_namespace
    let new_mnt_ns = if clone_flags & CloneFlags::CLONE_NEWNS.bits() != 0 {
        Arc::new(MntNamespace::default().create_mnt_namespace(user_ns.clone(), false)?)
    } else {
        pcb.get_nsproxy().read().mnt_namespace.clone()
    };
    nsproxy.set_mnt_namespace(new_mnt_ns);

    Ok(nsproxy)
}

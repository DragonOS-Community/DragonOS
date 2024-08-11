use alloc::string::ToString;

use super::ucount::UcountType::UCOUNT_PID_NAMESPACES;
use super::NsSet;
use super::{namespace::NsCommon, ucount::UCounts, user_namespace::UserNamespace};
use crate::container_of;
use crate::namespace::namespace::NsOperations;
use crate::process::fork::CloneFlags;
use crate::process::ProcessManager;
use crate::syscall::Syscall;
use crate::{libs::rwlock::RwLock, process::Pid};
use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use system_error::SystemError;
use system_error::SystemError::ENOSPC;

const MAX_PID_NS_LEVEL: u32 = 32;
const PIDNS_ADDING: u32 = 1 << 31;
#[repr(C)]
pub struct PidNamespace {
    /// 已经分配的进程数
    pid_allocated: u32,
    /// 当前的pid_namespace所在的层数
    level: u32,
    /// 父命名空间
    parent: Option<Arc<PidNamespace>>,
    /// 资源计数器
    ucounts: Arc<UCounts>,
    /// 关联的用户namespace
    user_ns: Arc<UserNamespace>,
    /// 回收孤儿进程的init进程
    child_reaper: Arc<RwLock<Pid>>, // 用pid，因为线程本来,就有需要根据Pid找到
    /// namespace共有部分
    ns_common: Arc<NsCommon>,
}
struct PidNsOperations {
    name: String,
    clone_flags: CloneFlags,
}
impl PidNsOperations {
    pub fn new(name: String) -> Self {
        Self {
            name,
            clone_flags: CloneFlags::CLONE_NEWPID,
        }
    }
}

impl NsOperations for PidNsOperations {
    fn put(&self, ns_common: Arc<NsCommon>) {
        let ns_common_ptr = Arc::as_ptr(&ns_common);
        let raw_pid_ns = container_of!(ns_common_ptr, PidNamespace, ns_common);
        let pid_ns = unsafe { Arc::from_raw(raw_pid_ns) };
        // pid_ns 超出作用域自动drop 同时递归drop
    }

    fn owner(&self, ns_common: Arc<NsCommon>) -> Arc<UserNamespace> {
        let ns_common_ptr = Arc::as_ptr(&ns_common);
        let pid_ns = unsafe { &*container_of!(ns_common_ptr, PidNamespace, ns_common) };
        pid_ns.user_ns.clone()
    }

    fn get_parent(&self, ns_common: Arc<NsCommon>) -> Arc<NsCommon> {
        unimplemented!()
    }

    fn get(&self, pid: Pid) -> Option<Arc<NsCommon>> {
        let pcb = ProcessManager::find(pid);
        pcb.and_then(|pcb| {
            pcb.get_nsproxy()
                .upgrade()
                .map(|nsproxy| nsproxy.pid_namespace.ns_common.clone())
        })
    }
    fn install(&self, nsset: Arc<NsSet>, ns_common: Arc<NsCommon>) -> u32 {
        let nsproxy = nsset.nsproxy.clone();
        let current = ProcessManager::current_pid();
        unimplemented!()
    }
}
impl PidNamespace {
    pub fn create_pid_namespace(
        &self,
        parent: Option<Arc<PidNamespace>>,
        user_ns: Arc<UserNamespace>,
    ) -> Result<Self, SystemError> {
        let level = parent.as_ref().map_or(0, |parent| parent.level + 1);
        if level > MAX_PID_NS_LEVEL {
            return Err(ENOSPC);
        }
        let ucounts = self.inc_pid_namespaces(user_ns.clone())?;

        if ucounts.is_none() {
            return Err(SystemError::ENOSPC);
        }
        let ucounts = ucounts.unwrap();

        let ns_common = Arc::new(NsCommon::new(Box::new(PidNsOperations::new(
            "pid".to_string(),
        )))?);
        Ok(Self {
            pid_allocated: PIDNS_ADDING,
            level,
            ucounts,
            parent,
            user_ns,
            ns_common,
            child_reaper: Arc::new(RwLock::new(Pid::new(1))), //默认为init进程，在clone的时候改变
        })
    }

    pub fn inc_pid_namespaces(
        &self,
        user_ns: Arc<UserNamespace>,
    ) -> Result<Option<Arc<UCounts>>, SystemError> {
        // 默认为root uid = 0
        Ok(self
            .ucounts
            .inc_ucounts(user_ns, Syscall::geteuid()? as u32, UCOUNT_PID_NAMESPACES))
    }

    pub fn dec_pid_namespaces(&mut self, uc: Arc<UCounts>) {
        UCounts::dec_ucount(uc, UCOUNT_PID_NAMESPACES)
    }
}

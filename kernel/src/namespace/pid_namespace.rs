use alloc::vec::Vec;

use super::namespace::Namespace;
use super::ucount::UcountType::UcountPidNamespaces;
use super::NsSet;
use super::{namespace::NsCommon, ucount::UCounts, user_namespace::UserNamespace};
use crate::container_of;
use crate::filesystem::vfs::{IndexNode, ROOT_INODE};
use crate::include::bindings::bindings::INT16_MAX;
use crate::namespace::namespace::NsOperations;
use crate::process::fork::CloneFlags;
use crate::process::ProcessManager;
use crate::syscall::Syscall;
use crate::{libs::rwlock::RwLock, process::Pid};
use alloc::boxed::Box;
use alloc::string::String;
use alloc::string::ToString;
use alloc::sync::Arc;
use ida::IdAllocator;
use system_error::SystemError;
use system_error::SystemError::ENOSPC;

const MAX_PID_NS_LEVEL: usize = 32;
const PIDNS_ADDING: u32 = 1 << 31;
const PID_MAX: usize = 4096;
static PID_IDA: ida::IdAllocator = ida::IdAllocator::new(1, usize::MAX);
#[derive(Debug)]
#[repr(C)]
pub struct PidNamespace {
    id_alloctor: IdAllocator,
    /// 已经分配的进程数
    pid_allocated: u32,
    /// 当前的pid_namespace所在的层数
    pub level: usize,
    /// 父命名空间
    parent: Option<Arc<PidNamespace>>,
    /// 资源计数器
    ucounts: Arc<UCounts>,
    /// 关联的用户namespace
    user_ns: Arc<UserNamespace>,
    /// 回收孤儿进程的init进程
    child_reaper: Arc<RwLock<PidStrcut>>,
    /// namespace共有部分
    pub ns_common: Arc<NsCommon>,
}
#[derive(Debug, Clone)]
pub struct PidStrcut {
    pub pid: Pid,
    pub level: usize,
    pub numbers: Vec<UPid>,
    pub stashed: Arc<dyn IndexNode>,
}
#[derive(Debug, Clone)]
pub struct UPid {
    pub nr: Pid, // 在某个pid_namespace 中的pid号
    pub ns: Arc<PidNamespace>,
}

impl UPid {
    pub fn get_ns(&self) -> Arc<PidNamespace> {
        self.ns.clone()
    }
}
impl PidStrcut {
    pub fn new() -> Self {
        Self {
            pid: Pid::new(1),
            level: 0,
            numbers: Vec::new(),
            stashed: ROOT_INODE(),
        }
    }

    pub fn put_pid(pid: Pid) {}

    pub fn alloc_pid(ns: Arc<PidNamespace>, set_tid: Vec<usize>) -> Result<PidStrcut, SystemError> {
        if set_tid.len() > ns.level + 1 {
            return Err(SystemError::EINVAL);
        }
        let mut numbers = Vec::<UPid>::with_capacity(ns.level as usize);
        let mut tid_iter = set_tid.into_iter().rev();
        let mut pid_ns = Some(ns.clone());
        for i in (0..=ns.level as usize).rev() {
            let tid = tid_iter.next().unwrap_or(0);
            if tid < 1 || tid > INT16_MAX as usize {
                return Err(SystemError::EINVAL);
            }
            let mut nr = tid;
            if tid == 0 {
                if let Some(ns) = pid_ns {
                    nr = if ns.id_alloctor.get_max_id() > PID_MAX {
                        PID_MAX
                    } else {
                        ns.id_alloctor.alloc().expect("No more id to allocate.")
                    };
                    pid_ns = ns.parent.clone();
                } else {
                    return Err(SystemError::EINVAL);
                }
            }
            numbers.insert(
                i,
                UPid {
                    nr: Pid::from(nr),
                    ns: ns.clone(),
                },
            );
        }

        Ok(Self {
            pid: numbers.last().unwrap().nr,
            level: ns.level,
            numbers,
            stashed: ROOT_INODE(),
        })
    }

    pub fn pid_to_ns(&self) -> Arc<PidNamespace> {
        self.numbers[self.level].get_ns()
    }
}
#[derive(Debug)]
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
impl Namespace for PidNamespace {
    fn ns_common_to_ns(ns_common: Arc<NsCommon>) -> Arc<Self> {
        container_of!(Arc::as_ptr(&ns_common), PidNamespace, ns_common)
    }
}

impl NsOperations for PidNsOperations {
    fn put(&self, ns_common: Arc<NsCommon>) {
        let _pid_ns = PidNamespace::ns_common_to_ns(ns_common);
        // pid_ns 超出作用域自动drop 同时递归drop
    }

    fn owner(&self, ns_common: Arc<NsCommon>) -> Arc<UserNamespace> {
        let pid_ns = PidNamespace::ns_common_to_ns(ns_common);
        pid_ns.user_ns.clone()
    }

    fn get_parent(&self, ns_common: Arc<NsCommon>) -> Result<Arc<NsCommon>, SystemError> {
        let current = ProcessManager::current_pid();
        let pcb = ProcessManager::find(current).unwrap();
        let active = pcb.pid_strcut().pid_to_ns();
        let mut pid_ns = &PidNamespace::ns_common_to_ns(ns_common).parent;

        while let Some(ns) = pid_ns {
            if Arc::ptr_eq(&active, &ns) {
                return Ok(ns.ns_common.clone());
            }
            pid_ns = &ns.parent;
        }
        Err(SystemError::EPERM)
    }

    fn get(&self, pid: Pid) -> Option<Arc<NsCommon>> {
        let pcb = ProcessManager::find(pid);
        pcb.and_then(|pcb| {
            pcb.get_nsproxy()
                .read()
                .pid_namespace
                .clone()
                .and_then(|ns| Some(ns.ns_common.clone()))
        })
    }
    fn install(&self, nsset: &mut NsSet, ns_common: Arc<NsCommon>) -> Result<(), SystemError> {
        let nsproxy = &mut nsset.nsproxy;
        let current = ProcessManager::current_pid();
        let pcb = ProcessManager::find(current).unwrap();
        let active = pcb.pid_strcut().pid_to_ns();
        let mut pid_ns = PidNamespace::ns_common_to_ns(ns_common);
        if pid_ns.level < active.level {
            return Err(SystemError::EINVAL);
        }
        while pid_ns.level > active.level {
            if let Some(ns) = &pid_ns.parent {
                pid_ns = ns.clone();
            } else {
                break;
            }
        }
        if Arc::ptr_eq(&pid_ns, &active) {
            return Err(SystemError::EINVAL);
        }
        nsproxy.pid_namespace = Some(pid_ns.clone());
        Ok(())
    }
}
impl PidNamespace {
    pub fn new() -> Result<Self, SystemError> {
        Ok(Self {
            id_alloctor: IdAllocator::new(1, PID_MAX),
            pid_allocated: 0,
            level: 0,
            child_reaper: Arc::new(RwLock::new(PidStrcut::new())),
            parent: None,
            ucounts: Arc::new(UCounts::new()?),
            user_ns: Arc::new(UserNamespace::new()?),
            ns_common: Arc::new(NsCommon::new(Box::new(PidNsOperations::new(
                "Pid".to_string(),
            )))?),
        })
    }

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
        let stashed = ROOT_INODE().find("proc")?;
        let child_reaper = if let Some(parent_ns) = &parent {
            parent_ns.child_reaper.clone()
        } else {
            Arc::new(RwLock::new(PidStrcut::new()))
        };
        Ok(Self {
            id_alloctor: IdAllocator::new(1, PID_MAX),
            pid_allocated: PIDNS_ADDING,
            level,
            ucounts,
            parent,
            user_ns,
            ns_common,
            child_reaper,
        })
    }

    pub fn inc_pid_namespaces(
        &self,
        user_ns: Arc<UserNamespace>,
    ) -> Result<Option<Arc<UCounts>>, SystemError> {
        Ok(self
            .ucounts
            .inc_ucounts(user_ns, Syscall::geteuid()? as u32, UcountPidNamespaces))
    }

    pub fn dec_pid_namespaces(&mut self, uc: Arc<UCounts>) {
        UCounts::dec_ucount(uc, UcountPidNamespaces)
    }
}

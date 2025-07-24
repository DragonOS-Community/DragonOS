use alloc::sync::Weak;

use alloc::{sync::Arc, vec::Vec};
use hashbrown::HashMap;
use ida::IdAllocator;
use system_error::SystemError;

use crate::libs::spinlock::SpinLock;
use crate::libs::spinlock::SpinLockGuard;
use crate::process::fork::CloneFlags;
use crate::process::pid::Pid;
use crate::process::ProcessControlBlock;
use crate::process::ProcessManager;
use crate::process::RawPid;

use super::nsproxy::NsCommon;
use super::user_namespace::UserNamespace;

lazy_static! {
    pub static ref INIT_PID_NAMESPACE: Arc<PidNamespace> = PidNamespace::new_root();
}
pub struct PidNamespace {
    self_ref: Weak<PidNamespace>,
    /// PID namespace的层级（root = 0）
    pub level: u32,
    /// 父namespace的弱引用
    parent: Option<Weak<PidNamespace>>,
    user_ns: Arc<UserNamespace>,

    inner: SpinLock<InnerPidNamespace>,
}

pub struct InnerPidNamespace {
    pub ns_common: NsCommon,
    ida: IdAllocator,
    /// PID到进程的映射表
    pid_map: HashMap<RawPid, Arc<Pid>>,
    /// init进程引用
    child_reaper: Option<Weak<ProcessControlBlock>>,
    children: Vec<Arc<PidNamespace>>,
}

impl PidNamespace {
    /// 最大PID namespace层级
    pub const MAX_PID_NS_LEVEL: u32 = 32;

    /// 创建root PID namespace
    fn new_root() -> Arc<Self> {
        Arc::new_cyclic(|self_ref| Self {
            self_ref: self_ref.clone(),
            level: 0,
            parent: None,
            user_ns: super::user_namespace::INIT_USER_NAMESPACE.clone(),
            inner: SpinLock::new(InnerPidNamespace {
                ns_common: NsCommon::default(),
                child_reaper: None,
                ida: IdAllocator::new(1, usize::MAX).unwrap(),
                pid_map: HashMap::new(),
                children: Vec::new(),
            }),
        })
    }

    pub fn alloc_pid_in_ns(&self, pid: Arc<Pid>) -> Result<RawPid, SystemError> {
        let mut inner = self.inner();
        let raw_pid = inner.do_alloc_pid_in_ns(pid)?;
        Ok(raw_pid)
    }

    pub fn pid_allocated(&self) -> usize {
        let inner = self.inner();
        inner.do_pid_allocated()
    }

    pub fn release_pid_in_ns(&self, raw_pid: RawPid) {
        let mut inner = self.inner();
        inner.do_release_pid_in_ns(raw_pid);
    }

    pub fn find_pid_in_ns(&self, raw_pid: RawPid) -> Option<Arc<Pid>> {
        let inner = self.inner();
        inner.pid_map.get(&raw_pid).cloned()
    }

    /// https://code.dragonos.org.cn/xref/linux-6.6.21/kernel/pid_namespace.c#145
    pub(super) fn copy_pid_ns(
        &self,
        clone_flags: &CloneFlags,
        user_ns: Arc<UserNamespace>,
    ) -> Result<Arc<Self>, SystemError> {
        if !clone_flags.contains(CloneFlags::CLONE_NEWPID) {
            return Ok(self.self_ref.upgrade().unwrap());
        }
        if !Arc::ptr_eq(
            &ProcessManager::current_pcb().active_pid_ns(),
            &self.self_ref.upgrade().unwrap(),
        ) {
            return Err(SystemError::EINVAL);
        }

        return self.create_pid_namespace(user_ns);
    }

    /// https://code.dragonos.org.cn/xref/linux-6.6.21/kernel/pid_namespace.c#72
    fn create_pid_namespace(&self, user_ns: Arc<UserNamespace>) -> Result<Arc<Self>, SystemError> {
        let level = self.level + 1;
        if !self.user_ns.is_ancestor_of(&user_ns) {
            return Err(SystemError::EINVAL);
        }

        if level > Self::MAX_PID_NS_LEVEL {
            return Err(SystemError::ENOSPC);
        }

        // todo: 补充ucount相关

        let pidns = Arc::new_cyclic(|self_ref| Self {
            self_ref: self_ref.clone(),
            level: level,
            parent: Some(self.self_ref.clone()),
            user_ns: user_ns,
            inner: SpinLock::new(InnerPidNamespace {
                ns_common: NsCommon::default(),
                child_reaper: None,
                ida: IdAllocator::new(1, usize::MAX).unwrap(),
                pid_map: HashMap::new(),
                children: Vec::new(),
            }),
        });

        // todo: procfs相关,申请inode号,赋值operations等

        self.inner().children.push(pidns.clone());
        return Ok(pidns);
    }

    pub fn inner(&self) -> SpinLockGuard<InnerPidNamespace> {
        self.inner.lock()
    }

    pub fn child_reaper(&self) -> Option<Weak<ProcessControlBlock>> {
        self.inner().child_reaper.clone()
    }

    pub fn set_child_reaper(&self, child_reaper: Weak<ProcessControlBlock>) {
        self.inner().child_reaper = Some(child_reaper);
    }

    pub fn parent(&self) -> Option<Arc<PidNamespace>> {
        self.parent.as_ref().and_then(|p| p.upgrade())
    }

    /// 从父namespace中删除当前PID namespace
    pub fn delete_current_pidns_in_parent(&self) {
        let current = self.self_ref.upgrade().unwrap();
        if let Some(p) = self.parent() {
            p.inner().children.retain(|c| !Arc::ptr_eq(&c, &current));
        }
    }
}

impl InnerPidNamespace {
    pub fn do_alloc_pid_in_ns(&mut self, pid: Arc<Pid>) -> Result<RawPid, SystemError> {
        let raw_pid = self.ida.alloc().ok_or(SystemError::ENOMEM)?;
        let raw_pid = RawPid(raw_pid);
        self.pid_map.insert(raw_pid, pid);
        Ok(raw_pid)
    }

    pub fn do_release_pid_in_ns(&mut self, raw_pid: RawPid) {
        self.pid_map.remove(&raw_pid);
        self.ida.free(raw_pid.data());
    }

    pub fn do_pid_allocated(&self) -> usize {
        self.pid_map.len()
    }
}

impl ProcessControlBlock {
    pub fn active_pid_ns(&self) -> Arc<PidNamespace> {
        self.pid().ns_of_pid()
    }
}

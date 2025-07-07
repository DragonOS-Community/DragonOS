use alloc::sync::Weak;

use alloc::sync::Arc;
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

pub struct PidNamespace {
    self_ref: Weak<PidNamespace>,
    /// PID namespace的层级（root = 0）
    pub level: u32,
    /// 父namespace的弱引用
    parent: Option<Weak<PidNamespace>>,

    inner: SpinLock<InnerPidNamespace>,
}

pub struct InnerPidNamespace {
    pub ns_common: NsCommon,
    ida: IdAllocator,
    /// PID到进程的映射表
    pid_map: HashMap<RawPid, Arc<Pid>>,
    /// init进程引用
    child_reaper: Option<Weak<ProcessControlBlock>>,
}

impl PidNamespace {
    /// 创建root PID namespace
    pub fn new_root() -> Arc<Self> {
        Arc::new_cyclic(|self_ref| Self {
            self_ref: self_ref.clone(),
            level: 0,
            parent: None,
            inner: SpinLock::new(InnerPidNamespace {
                ns_common: NsCommon::default(),
                child_reaper: None,
                ida: IdAllocator::new(1, usize::MAX).unwrap(),
                pid_map: HashMap::new(),
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
        todo!("Implement PID namespace creation logic with user namespace support");
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
        self.pid().unwrap().ns_of_pid()
    }
}

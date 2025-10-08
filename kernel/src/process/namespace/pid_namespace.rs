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
use super::{NamespaceOps, NamespaceType};

lazy_static! {
    pub static ref INIT_PID_NAMESPACE: Arc<PidNamespace> = PidNamespace::new_root();
}

#[derive(Debug)]
pub struct PidNamespace {
    ns_common: NsCommon,
    self_ref: Weak<PidNamespace>,
    /// 父namespace的弱引用
    parent: Option<Weak<PidNamespace>>,
    user_ns: Arc<UserNamespace>,

    inner: SpinLock<InnerPidNamespace>,
}

#[derive(Debug)]
pub struct InnerPidNamespace {
    dead: bool,
    ida: IdAllocator,
    /// PID到进程的映射表
    pid_map: HashMap<RawPid, Arc<Pid>>,
    /// init进程引用
    child_reaper: Option<Weak<ProcessControlBlock>>,
    children: Vec<Arc<PidNamespace>>,
}

impl InnerPidNamespace {
    pub fn dead(&self) -> bool {
        self.dead
    }

    pub fn child_reaper(&self) -> &Option<Weak<ProcessControlBlock>> {
        &self.child_reaper
    }
}

impl NamespaceOps for PidNamespace {
    fn ns_common(&self) -> &NsCommon {
        &self.ns_common
    }
}

impl PidNamespace {
    /// 最大PID namespace层级
    pub const MAX_PID_NS_LEVEL: u32 = 32;

    /// 创建root PID namespace
    fn new_root() -> Arc<Self> {
        Arc::new_cyclic(|self_ref| Self {
            self_ref: self_ref.clone(),
            ns_common: NsCommon::new(0, NamespaceType::Pid),
            parent: None,
            user_ns: super::user_namespace::INIT_USER_NAMESPACE.clone(),
            inner: SpinLock::new(InnerPidNamespace {
                dead: false,
                child_reaper: None,
                ida: IdAllocator::new(1, usize::MAX).unwrap(),
                pid_map: HashMap::new(),
                children: Vec::new(),
            }),
        })
    }

    /// 获取层级
    pub fn level(&self) -> u32 {
        self.ns_common.level
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
    pub(super) fn create_pid_namespace(
        &self,
        user_ns: Arc<UserNamespace>,
    ) -> Result<Arc<Self>, SystemError> {
        let level = self.level() + 1;
        if !self.user_ns.is_ancestor_of(&user_ns) {
            return Err(SystemError::EINVAL);
        }

        if level > Self::MAX_PID_NS_LEVEL {
            return Err(SystemError::ENOSPC);
        }

        // todo: 补充ucount相关

        let pidns = Arc::new_cyclic(|self_ref| Self {
            self_ref: self_ref.clone(),
            ns_common: NsCommon::new(level, NamespaceType::Pid),
            parent: Some(self.self_ref.clone()),
            user_ns,
            inner: SpinLock::new(InnerPidNamespace {
                child_reaper: None,
                dead: false,
                ida: IdAllocator::new(1, usize::MAX).unwrap(),
                pid_map: HashMap::new(),
                children: Vec::new(),
            }),
        });

        // todo: procfs相关,申请inode号,赋值operations等

        self.inner().children.push(pidns.clone());
        return Ok(pidns);
    }

    pub fn inner(&self) -> SpinLockGuard<'_, InnerPidNamespace> {
        self.inner.lock()
    }

    pub fn child_reaper(&self) -> Option<Weak<ProcessControlBlock>> {
        self.inner().child_reaper.clone()
    }

    pub fn set_child_reaper(&self, child_reaper: Weak<ProcessControlBlock>) {
        self.inner().child_reaper = Some(child_reaper);
    }

    /// 获取IDA分配器已使用的ID数量
    pub fn ida_used(&self) -> usize {
        self.inner().ida.used()
    }

    /// 获取IDA分配器当前ID
    pub fn ida_current_id(&self) -> usize {
        self.inner().ida.current_id()
    }

    pub fn parent(&self) -> Option<Arc<PidNamespace>> {
        self.parent.as_ref().and_then(|p| p.upgrade())
    }

    /// 从父namespace中删除当前PID namespace
    pub fn delete_current_pidns_in_parent(&self) {
        let current = self.self_ref.upgrade().unwrap();
        if let Some(p) = self.parent() {
            p.inner().children.retain(|c| !Arc::ptr_eq(c, &current));
        }
    }

    /// 获取当前 PID namespace 中的所有进程 ID
    pub fn get_all_pids(&self) -> Vec<RawPid> {
        let inner = self.inner();
        inner.pid_map.keys().copied().collect()
    }

    /// 检查指定的 PID 是否在当前 namespace 中
    pub fn contains_pid(&self, raw_pid: RawPid) -> bool {
        let inner = self.inner();
        inner.pid_map.contains_key(&raw_pid)
    }

    /// 正确的方式来标记PID namespace为dead
    /// 只有当init进程（PID 1）退出时才应该调用
    pub fn mark_dead_on_init_exit(&self) {
        let mut inner = self.inner();
        if !inner.dead {
            log::debug!("[PID_NAMESPACE] Marking namespace level {} as dead due to init process exit", self.level());
            inner.dead = true;
        }
    }

    /// 检查namespace是否为dead状态
    pub fn is_dead(&self) -> bool {
        self.inner().dead
    }
}

impl InnerPidNamespace {
    pub fn do_alloc_pid_in_ns(&mut self, pid: Arc<Pid>) -> Result<RawPid, SystemError> {
        if self.dead {
            return Err(SystemError::ESRCH);
        }
        let raw_pid = self.ida.alloc().ok_or(SystemError::ENOMEM)?;
        let raw_pid = RawPid(raw_pid);
        // log::debug!("do_alloc_pid_in_ns: allocated raw_pid={}, ida.used={}", raw_pid, self.ida.used());
        self.pid_map.insert(raw_pid, pid);
        Ok(raw_pid)
    }

    pub fn do_release_pid_in_ns(&mut self, raw_pid: RawPid) {
        // 注意：self是InnerPidNamespace，无法直接获取level信息
        // level信息需要在调用此函数的外部记录
        
        // 原有的PID映射移除逻辑，添加调试信息
        let _existed_before = self.pid_map.contains_key(&raw_pid);
        self.pid_map.remove(&raw_pid);
        // log::debug!("[PID_RELEASE] PID {} removed from pid_map: existed_before={}", raw_pid, _existed_before);
        
        // IDA释放前后的状态记录
        let _ida_used_before = self.ida.used();
        self.ida.free(raw_pid.data());
        let _ida_used_after = self.ida.used();
        // log::debug!("[PID_RELEASE] IDA free operation: raw_pid={}, ida.used_before={}, ida.used_after={}", 
        //            raw_pid, _ida_used_before, _ida_used_after);
        
        // 移除错误的dead标记逻辑
        // PID namespace不应该因为临时进程退出而被标记为dead
        // 只有当init进程退出或namespace被显式销毁时才应该标记为dead
        // 这修复了PID复用问题：当namespace被错误标记为dead时，
        // 后续的PID分配会返回ESRCH，导致PID无法复用
        
        // log::debug!("[PID_RELEASE] do_release_pid_in_ns completed: raw_pid={}, final_ida.used={}", 
        //            raw_pid, self.ida.used());
    }

    pub fn do_pid_allocated(&self) -> usize {
        self.pid_map.len()
    }
}

impl Drop for PidNamespace {
    fn drop(&mut self) {
        // log::debug!("Dropping PidNamespace at level {}", self.level);
    }
}

impl ProcessControlBlock {
    pub fn active_pid_ns(&self) -> Arc<PidNamespace> {
        self.pid().ns_of_pid()
    }
}

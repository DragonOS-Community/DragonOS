use alloc::sync::Weak;

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicU64, Ordering};
use hashbrown::HashMap;
use ida::IdAllocator;
use system_error::SystemError;

use crate::libs::spinlock::SpinLock;
use crate::libs::spinlock::SpinLockGuard;
use crate::process::fork::CloneFlags;
use crate::process::pid::{Pid, PidType};
use crate::process::ProcessControlBlock;
use crate::process::ProcessManager;
use crate::process::RawPid;

use super::nsproxy::NsCommon;
use super::user_namespace::UserNamespace;
use super::{NamespaceOps, NamespaceType};

lazy_static! {
    pub static ref INIT_PID_NAMESPACE: Arc<PidNamespace> = PidNamespace::new_root();
}

pub struct PidNamespace {
    ns_common: NsCommon,
    self_ref: Weak<PidNamespace>,
    /// 父namespace的弱引用
    parent: Option<Weak<PidNamespace>>,
    user_ns: Arc<UserNamespace>,
    processes_created: AtomicU64,

    inner: SpinLock<InnerPidNamespace>,
}

pub struct InnerPidNamespace {
    dead: bool,
    last_pid: RawPid,
    /// Highest successfully published ID, independent of cyclic allocation.
    published_high_water: usize,
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
    pub(in crate::process) fn new_root() -> Arc<Self> {
        Arc::new_cyclic(|self_ref| Self {
            self_ref: self_ref.clone(),
            ns_common: NsCommon::new(0, NamespaceType::Pid),
            parent: None,
            user_ns: super::user_namespace::INIT_USER_NAMESPACE.clone(),
            processes_created: AtomicU64::new(0),
            inner: SpinLock::new(InnerPidNamespace {
                dead: false,
                last_pid: RawPid(0),
                published_high_water: 0,
                child_reaper: None,
                ida: IdAllocator::new(1, crate::process::pid::PID_MAX_LIMIT).unwrap(),
                pid_map: HashMap::new(),
                children: Vec::new(),
            }),
        })
    }

    /// 获取层级
    pub fn level(&self) -> u32 {
        self.ns_common.level
    }

    pub fn user_ns(&self) -> &Arc<UserNamespace> {
        &self.user_ns
    }

    /// Caller owns PID_MAP_LOCK; reserve an invisible ID, not a Pid mapping.
    pub(in crate::process) fn reserve_pid_in_ns(
        &self,
        requested: Option<RawPid>,
        limit: usize,
    ) -> Result<RawPid, SystemError> {
        let mut inner = self.inner();
        if inner.dead {
            return Err(SystemError::ENOMEM);
        }
        if let Some(nr) = requested {
            return inner
                .ida
                .reserve_specific(nr.data())
                .map(RawPid)
                .ok_or(SystemError::EEXIST);
        }
        // Linux's IDR cursor is the next candidate; this namespace keeps
        // its last automatic selection for /proc/loadavg instead.
        let min = if inner.last_pid.data() >= 300 { 300 } else { 1 };
        let next = inner.last_pid.data().saturating_add(1);
        let nr = inner
            .ida
            .alloc_range_from(next, min, limit)
            .ok_or(SystemError::EAGAIN_OR_EWOULDBLOCK)?;
        inner.last_pid = RawPid(nr);
        Ok(RawPid(nr))
    }

    pub(in crate::process) fn publish_reserved_pid(&self, nr: RawPid, pid: Arc<Pid>) {
        let mut inner = self.inner();
        debug_assert!(!inner.dead && inner.ida.exists(nr.data()));
        debug_assert!(!inner.pid_map.contains_key(&nr));
        inner.pid_map.insert(nr, pid);
        inner.published_high_water = inner.published_high_water.max(nr.data());
        self.processes_created.fetch_add(1, Ordering::Relaxed);
    }

    pub(in crate::process) fn rollback_reserved_pid(&self, nr: RawPid) {
        self.inner().ida.free(nr.data());
    }

    pub(in crate::process) fn reset_fresh_pid_cursor(&self) {
        let mut inner = self.inner();
        if inner.pid_map.is_empty() && !inner.dead {
            inner.ida.reset_cursor();
            inner.last_pid = RawPid(0);
        }
    }

    pub fn pid_allocated(&self) -> usize {
        let inner = self.inner();
        inner.do_pid_allocated()
    }

    pub fn processes_created(&self) -> u64 {
        self.processes_created.load(Ordering::Relaxed)
    }

    pub fn last_pid(&self) -> RawPid {
        self.inner().last_pid
    }

    pub(in crate::process) fn release_pid_in_ns(&self, nr: RawPid) {
        let retired = {
            let _transaction = crate::process::pid::pid_map_lock();
            self.inner().do_release_pid_in_ns(nr)
        };
        drop(retired);
    }

    pub fn find_pid_in_ns(&self, raw_pid: RawPid) -> Option<Arc<Pid>> {
        let inner = self.inner();
        inner.pid_map.get(&raw_pid).cloned()
    }

    pub fn collect_pids(&self) -> Vec<Arc<Pid>> {
        let inner = self.inner();
        inner.pid_map.values().cloned().collect()
    }

    /// Fixed traversal bound: later forks cannot indefinitely extend a reader.
    pub(crate) fn published_pid_bound(&self) -> usize {
        self.inner().published_high_water
    }

    /// Advance through published numeric identities with bounded lock work.
    /// The caller supplies empty slots and retires returned owners outside locks.
    /// No task/PID-link/cgroup lock is acquired while holding the namespace lock.
    pub(crate) fn read_pid_batch(
        &self,
        next: &mut usize,
        bound: usize,
        batch: &mut [Option<Arc<Pid>>],
    ) -> usize {
        let inner = self.inner();
        let mut count = 0;
        let end = next.saturating_add(256).min(bound.saturating_add(1));
        while *next < end && count < batch.len() {
            if let Some(pid) = inner.pid_map.get(&RawPid(*next)) {
                debug_assert!(batch[count].is_none());
                batch[count] = Some(pid.clone());
                count += 1;
            }
            *next += 1;
        }
        count
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
            processes_created: AtomicU64::new(0),
            inner: SpinLock::new(InnerPidNamespace {
                child_reaper: None,
                dead: false,
                last_pid: RawPid(0),
                published_high_water: 0,
                ida: IdAllocator::new(1, crate::process::pid::PID_MAX_LIMIT).unwrap(),
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
        let _transaction = crate::process::pid::pid_map_lock();
        self.inner().child_reaper = Some(child_reaper);
    }

    pub fn disable_pid_allocation(&self) {
        let _transaction = crate::process::pid::pid_map_lock();
        self.inner().dead = true;
    }

    /// Cancel a namespace created by this failed fork/unshare attempt. The
    /// caller must retain an Arc and must not pass an inherited namespace:
    /// the parent's children list owns an extra reference even before PID 1.
    pub(in crate::process) fn discard_unpublished(&self) {
        self.disable_pid_allocation();
        self.delete_current_pidns_in_parent();
    }

    pub(in crate::process) fn selftest_discard_unpublished() -> Result<bool, SystemError> {
        let parent = Self::new_root();
        for _ in 0..32 {
            let child = parent.create_pid_namespace(parent.user_ns.clone())?;
            let weak = Arc::downgrade(&child);
            let retained = parent.inner().children.len() == 1;
            child.discard_unpublished();
            // Error cleanup can run again after PID cleanup removed the
            // parent entry. It must remain safe and release the last owner.
            child.discard_unpublished();
            drop(child);
            if !retained || weak.upgrade().is_some() || !parent.inner().children.is_empty() {
                return Ok(false);
            }
        }
        Ok(true)
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
}

impl InnerPidNamespace {
    pub fn do_release_pid_in_ns(&mut self, raw_pid: RawPid) -> Option<Arc<Pid>> {
        // log::debug!("do_release_pid_in_ns: raw_pid={}", raw_pid);
        let retired = self.pid_map.remove(&raw_pid);
        self.ida.free(raw_pid.data());
        if self.pid_map.is_empty() {
            // 如果当前namespace中没有任何PID了，则标记为dead
            self.dead = true;
        }
        retired
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
    pub fn try_active_pid_ns(&self) -> Option<Arc<PidNamespace>> {
        self.task_pid_ptr(PidType::PID)
            .and_then(|pid| pid.try_ns_of_pid())
    }

    pub fn active_pid_ns(&self) -> Arc<PidNamespace> {
        self.pid().ns_of_pid()
    }
}

impl ProcessManager {
    pub fn current_pidns() -> Arc<PidNamespace> {
        Self::current_pcb().active_pid_ns()
    }
}

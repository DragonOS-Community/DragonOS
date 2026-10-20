use core::fmt::Debug;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::filesystem::epoll::{
    event_poll::{EPollItemList, EventPoll},
    EPollEventType, EPollItem,
};
use crate::libs::rwlock::RwLock;
use crate::libs::spinlock::{SpinLock, SpinLockGuard};
use crate::process::ProcessManager;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use system_error::SystemError;

use super::namespace::pid_namespace::PidNamespace;
use super::{ProcessControlBlock, RawPid};

#[path = "pid_selftest.rs"]
mod selftest;
pub(crate) use selftest::selftest_membership_lifetime;

/// Serializes logical and physical PGID/SID membership transactions.
///
/// This is the DragonOS equivalent of the Linux tasklist lock coverage used by
/// setpgid(), setsid(), de_thread(), exit unhashing, and process-group lookup.
static PID_MEMBERSHIP_LOCK: SpinLock<()> = SpinLock::new(());

// Distinct from task membership: free_pid can already run under that lock.
// Order: membership -> map transaction -> one namespace inner at a time.
static PID_MAP_LOCK: SpinLock<()> = SpinLock::new(());
pub(super) fn pid_map_lock() -> SpinLockGuard<'static, ()> {
    PID_MAP_LOCK.lock_irqsave()
}

pub(crate) const PID_MAX_LIMIT: usize = 4_194_304;
static PID_MAX: AtomicUsize = AtomicUsize::new(32_768);
static PID_MAX_MIN: AtomicUsize = AtomicUsize::new(301);

pub(crate) fn init_pid_limits() {
    let cpus = if crate::smp::cpu::smp_cpu_manager_initialized() {
        crate::smp::cpu::smp_cpu_manager()
            .possible_cpus_count()
            .max(1) as usize
    } else {
        1
    };
    PID_MAX_MIN.store(
        (cpus.saturating_mul(8)).clamp(301, PID_MAX_LIMIT),
        Ordering::Release,
    );
    PID_MAX.store(
        (cpus.saturating_mul(1024)).clamp(32_768, PID_MAX_LIMIT),
        Ordering::Release,
    );
}

pub(crate) fn pid_max() -> usize {
    PID_MAX.load(Ordering::Acquire)
}

pub(crate) fn set_pid_max(value: usize) -> Result<(), SystemError> {
    if !(PID_MAX_MIN.load(Ordering::Acquire)..=PID_MAX_LIMIT).contains(&value) {
        return Err(SystemError::EINVAL);
    }
    PID_MAX.store(value, Ordering::Release);
    Ok(())
}

pub(crate) fn pid_membership_lock() -> SpinLockGuard<'static, ()> {
    PID_MEMBERSHIP_LOCK.lock_irqsave()
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PidType {
    /// pid类型是进程id
    PID = 1,
    TGID = 2,
    PGID = 3,
    SID = 4,
    MAX = 5,
}

impl PidType {
    pub const ALL: [PidType; Self::PIDTYPE_MAX - 1] =
        [PidType::PID, PidType::TGID, PidType::PGID, PidType::SID];
    pub const PIDTYPE_MAX: usize = PidType::MAX as usize;
}

/// 每个进程的 pid 私有信息, 通常作为 pidfd 的 private_data
/// TODO: 未实现完, 参考https://code.dragonos.org.cn/xref/linux-6.1.9/include/linux/pid.h#59
/// TODO: 应该替换所有的 pid 相关使用, 目前内核是直接使用传入的 pid, 应该全部转换为使用此结构体
/// 例如 struct pid 应该是在进程创建时（如 fork(), clone()）必然创建的
#[derive(Clone, Debug)]
pub struct PidPrivateData {
    pid: Arc<Pid>,
}

impl PidPrivateData {
    pub fn new(pid: Arc<Pid>) -> Self {
        Self { pid }
    }

    pub fn pid(&self) -> Arc<Pid> {
        self.pid.clone()
    }

    pub fn pid_nr_ns(&self, ns: &Arc<PidNamespace>) -> RawPid {
        self.pid.pid_nr_ns(ns)
    }
}

pub struct Pid {
    self_ref: Weak<Pid>,
    pub level: u32,
    dead: AtomicBool,
    /// 使用此PID的任务列表，按PID类型分组
    /// tasks[PidType::PID as usize] = 使用该PID作为进程ID的任务
    /// tasks[PidType::TGID as usize] = 使用该PID作为线程组ID的任务
    /// Timer callbacks resolve their saved TGID here. All readers and writers
    /// must disable local IRQs so a callback cannot interrupt a lock owner.
    tasks: [SpinLock<Vec<Weak<ProcessControlBlock>>>; PidType::PIDTYPE_MAX],
    /// PID numbers in each namespace. These remain as identity until final drop.
    numbers: SpinLock<Vec<Option<UPid>>>,
    /// Owns namespace map registrations and allocator slots until unregistration.
    registered: Vec<AtomicBool>,
    /// pidfd poll/epoll waiters. Linux keeps this wakeup edge on struct pid.
    pidfd_epitems: EPollItemList,
}

impl Debug for Pid {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Pid").finish()
    }
}

impl Pid {
    fn new(level: u32) -> Arc<Self> {
        let pid = Arc::new_cyclic(|weak_self| Self {
            self_ref: weak_self.clone(),
            dead: AtomicBool::new(false),
            level,
            tasks: core::array::from_fn(|_| SpinLock::new(Vec::new())),
            numbers: SpinLock::new(vec![None; level as usize + 1]),
            registered: (0..=level).map(|_| AtomicBool::new(false)).collect(),
            pidfd_epitems: EPollItemList::default(),
        });

        pid
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(nr: RawPid, ns: Arc<PidNamespace>) -> Arc<Self> {
        let pid = Self::new(ns.level());
        pid.numbers.lock()[ns.level() as usize] = Some(UPid::new(nr, ns));
        pid
    }

    #[cfg(test)]
    pub(crate) fn clear_numbers_for_test(&self) {
        for number in self.numbers.lock().iter_mut() {
            number.take();
        }
    }

    pub fn dead(&self) -> bool {
        self.dead.load(core::sync::atomic::Ordering::Relaxed)
    }

    pub fn set_dead(&self) {
        self.dead.store(true, core::sync::atomic::Ordering::Relaxed);
    }

    pub(crate) fn add_pidfd_epitem(&self, epitem: Arc<EPollItem>) {
        self.pidfd_epitems.add(epitem);
    }

    pub(crate) fn remove_pidfd_epitem(&self, epitem: &Arc<EPollItem>) -> Result<(), SystemError> {
        self.pidfd_epitems.remove(epitem)
    }

    pub(crate) fn wake_pidfd_pollers(&self) {
        let _ = EventPoll::wakeup_epoll(
            &self.pidfd_epitems,
            EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM,
        );
    }

    /// 获取指定PID所属的命名空间
    ///
    /// 返回该PID被分配时所在的PID命名空间的引用(Arc封装)
    pub fn try_ns_of_pid(&self) -> Option<Arc<PidNamespace>> {
        self.numbers
            .lock()
            .get(self.level as usize)
            .and_then(|upid| upid.as_ref().map(|upid| upid.ns.clone()))
    }

    pub fn ns_of_pid(&self) -> Arc<PidNamespace> {
        self.try_ns_of_pid()
            .expect("ns_of_pid requires an attached pid")
    }

    pub fn first_upid(&self) -> Option<UPid> {
        self.numbers.lock().first().cloned().flatten()
    }

    pub fn tasks_iter(&self, pid_type: PidType) -> PidTaskIterator<'_> {
        let guard = self.tasks[pid_type as usize].lock_irqsave();
        PidTaskIterator { guard, index: 0 }
    }

    /// Snapshot stable task references without allocating or notifying tasks
    /// under the PID membership spinlock. Membership can grow while reserving.
    pub(crate) fn try_tasks_snapshot(
        &self,
        pid_type: PidType,
    ) -> Result<Vec<Arc<ProcessControlBlock>>, SystemError> {
        let mut tasks = Vec::new();
        loop {
            let members = self.tasks[pid_type as usize].lock_irqsave();
            if tasks.capacity() < members.len() {
                let required = members.len();
                drop(members);
                tasks
                    .try_reserve(required)
                    .map_err(|_| SystemError::ENOMEM)?;
                continue;
            }
            for member in members.iter() {
                if let Some(task) = member.upgrade() {
                    tasks.push(task);
                }
            }
            return Ok(tasks);
        }
    }

    /// 判断当前pid是否是当前命名空间的init进程(即child reaper)
    ///
    /// 由于在copy_process中可能在pid_ns->child_reaper被赋值前就需要检查，
    /// 因此这里通过pid号来检查。
    /// 如果当前pid在当前命名空间中的pid号为1，则返回true，否则返回false。
    pub fn is_child_reaper(&self) -> bool {
        self.numbers.lock()[self.level as usize]
            .as_ref()
            .unwrap()
            .nr
            .data()
            == 1
    }

    pub fn has_task(&self, pid_type: PidType) -> bool {
        let tasks = self.tasks[pid_type as usize].lock_irqsave();
        !tasks.is_empty()
    }

    pub fn pid_task(&self, pid_type: PidType) -> Option<Arc<ProcessControlBlock>> {
        let tasks = self.tasks[pid_type as usize].lock_irqsave();
        if tasks.is_empty() {
            None
        } else {
            // 返回第一个进程
            tasks.first().and_then(|task| task.upgrade())
        }
    }

    /// Copy an observer while locked; upgrade and retire it outside PID locks.
    pub(crate) fn first_task_weak(&self, pid_type: PidType) -> Option<Weak<ProcessControlBlock>> {
        self.tasks[pid_type as usize]
            .lock_irqsave()
            .first()
            .cloned()
    }

    pub fn thread_group_leader_task(&self) -> Option<Arc<ProcessControlBlock>> {
        self.tasks_iter(PidType::TGID)
            .find(|task| task.is_thread_group_leader())
    }

    pub(crate) fn thread_group_exited_for_pidfd(&self) -> bool {
        let mut leader_seen = false;
        let mut exited_leader_seen = false;

        for task in self.tasks_iter(PidType::TGID) {
            if task.is_thread_group_leader() {
                leader_seen = true;
                if task.is_live_thread_group_member() {
                    return false;
                }
                if task.is_zombie() || task.is_dead() {
                    exited_leader_seen = true;
                }
                continue;
            }

            if task.is_live_thread_group_member() {
                return false;
            }
        }

        !leader_seen || exited_leader_seen
    }

    pub fn pid_vnr(&self) -> RawPid {
        let active_pid_ns = ProcessManager::current_pcb().active_pid_ns();
        self.pid_nr_ns(&active_pid_ns)
    }

    /// 获取在指定namespace中的PID号
    ///
    /// 如果当前PID在指定namespace中不存在，则返回0。
    ///
    /// 参考 https://code.dragonos.org.cn/xref/linux-6.6.21/kernel/pid.c#475
    pub fn pid_nr_ns(&self, ns: &Arc<PidNamespace>) -> RawPid {
        if ns.level() <= self.level {
            let numbers = self.numbers.lock();
            if let Some(upid) = numbers.get(ns.level() as usize).and_then(|x| x.as_ref()) {
                if Arc::ptr_eq(&upid.ns, ns) {
                    return upid.nr;
                }
            }
        }

        // 如果没有找到对应的UPid，返回0
        RawPid::new(0)
    }
}

impl PartialEq for Pid {
    fn eq(&self, other: &Self) -> bool {
        // 比较 `self_ref` 的指针地址（Weak 比较需要先升级为 Arc）
        if let (Some(self_arc), Some(other_arc)) =
            (self.self_ref.upgrade(), other.self_ref.upgrade())
        {
            Arc::ptr_eq(&self_arc, &other_arc)
        } else {
            false
        }
    }
}

impl Eq for Pid {}

impl Drop for Pid {
    fn drop(&mut self) {
        // Do not acquire the map transaction while holding the identity lock.
        let numbers = core::mem::take(&mut *self.numbers.lock());
        for (level, upid) in numbers.iter().enumerate() {
            if self.registered[level].swap(false, Ordering::AcqRel) {
                let upid = upid
                    .as_ref()
                    .expect("registered PID namespace entry must have an identity");
                upid.ns.release_pid_in_ns(upid.nr);
            }
        }
    }
}

pub struct PidTaskIterator<'a> {
    guard: SpinLockGuard<'a, Vec<Weak<ProcessControlBlock>>>,
    index: usize,
}

impl Iterator for PidTaskIterator<'_> {
    type Item = Arc<ProcessControlBlock>;

    fn next(&mut self) -> Option<Self::Item> {
        while self.index < self.guard.len() {
            if let Some(task) = self.guard[self.index].upgrade() {
                self.index += 1;
                return Some(task);
            }
            self.index += 1;
        }
        None
    }
}

/// 在特定namespace中的PID信息
#[derive(Clone)]
pub struct UPid {
    /// 在该namespace中的PID值
    pub nr: RawPid,
    /// 所属的namespace
    pub ns: Arc<PidNamespace>,
}

impl UPid {
    /// 创建新的UPid
    pub fn new(nr: RawPid, ns: Arc<PidNamespace>) -> Self {
        Self { nr, ns }
    }
}

impl Debug for UPid {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("UPid").field("nr", &self.nr).finish()
    }
}

impl ProcessControlBlock {
    pub fn pid(&self) -> Arc<Pid> {
        self.thread_pid.read().clone().unwrap()
    }

    #[cfg(test)]
    pub(crate) fn install_pid_identity_for_test(&self, pid: Arc<Pid>) {
        let raw_pid = pid
            .first_upid()
            .expect("test PID identity must have a namespace number")
            .nr;
        self.pid.store(raw_pid, Ordering::Release);
        self.thread_pid.write().replace(pid.clone());
        self.process_signal().set_pid(PidType::TGID, Some(pid));
    }

    #[cfg(test)]
    pub(crate) fn clear_pid_identity_for_test(&self) {
        self.process_signal().set_pid(PidType::TGID, None);
        self.thread_pid.write().take();
        self.pid.store(RawPid::new(0), Ordering::Release);
    }

    /// 强制设置当前进程的raw_pid
    /// 注意：这个函数应该在创建进程时调用，不能在运行时随意调用
    pub(super) unsafe fn force_set_raw_pid(&self, pid: RawPid) {
        self.pid.store(pid, Ordering::Release);
    }

    pub(super) fn exchange_tid_with<F>(
        self: &Arc<Self>,
        other: &Arc<ProcessControlBlock>,
        commit_outer_identity: F,
    ) where
        F: FnOnce(),
    {
        assert!(
            !Arc::ptr_eq(self, other),
            "cannot exchange one task's PID identity with itself"
        );

        let (first_task, second_task) = order_pcbs(self, other);
        let mut first_link = first_task.pid_links[PidType::PID as usize].pid.write();
        let mut second_link = second_task.pid_links[PidType::PID as usize].pid.write();

        let mut first_thread_pid = first_task.thread_pid.write();
        let mut second_thread_pid = second_task.thread_pid.write();

        let first_pid = first_thread_pid
            .clone()
            .expect("PID identity exchange requires first task thread_pid");
        let second_pid = second_thread_pid
            .clone()
            .expect("PID identity exchange requires second task thread_pid");
        assert!(
            !Arc::ptr_eq(&first_pid, &second_pid),
            "PID identity exchange requires distinct struct Pid objects"
        );
        assert!(
            first_link
                .as_ref()
                .map(|pid| Arc::ptr_eq(pid, &first_pid))
                .unwrap_or(false),
            "first task's PID link does not match thread_pid"
        );
        assert!(
            second_link
                .as_ref()
                .map(|pid| Arc::ptr_eq(pid, &second_pid))
                .unwrap_or(false),
            "second task's PID link does not match thread_pid"
        );

        let (first_pid_lock, second_pid_lock) = order_pids(&first_pid, &second_pid);
        let mut first_tasks = first_pid_lock.tasks[PidType::PID as usize].lock_irqsave();
        let mut second_tasks = second_pid_lock.tasks[PidType::PID as usize].lock_irqsave();

        let (first_task_for_pid, second_task_for_pid) = if Arc::ptr_eq(first_pid_lock, &first_pid) {
            (first_task, second_task)
        } else {
            (second_task, first_task)
        };

        // Locate both entries before changing either list.  Once the first
        // replacement is visible this operation is the committed half of an
        // exec identity transaction and must no longer fail.
        let first_index = find_task_in_pid_tasks(&first_tasks, first_task_for_pid)
            .expect("first task missing from validated struct Pid task list");
        let second_index = find_task_in_pid_tasks(&second_tasks, second_task_for_pid)
            .expect("second task missing from validated struct Pid task list");

        first_tasks[first_index] = Arc::downgrade(second_task_for_pid);
        second_tasks[second_index] = Arc::downgrade(first_task_for_pid);

        core::mem::swap(&mut *first_link, &mut *second_link);
        core::mem::swap(&mut *first_thread_pid, &mut *second_thread_pid);

        // Keep every PCB/Pid identity guard alive while the caller commits
        // raw-PID lookup and ptrace indices. Readers can then observe either
        // the complete old identity or the complete new identity, never a
        // Pid.tasks-only half commit.
        commit_outer_identity();
    }
}

fn order_pcbs<'a>(
    left: &'a Arc<ProcessControlBlock>,
    right: &'a Arc<ProcessControlBlock>,
) -> (&'a Arc<ProcessControlBlock>, &'a Arc<ProcessControlBlock>) {
    if (Arc::as_ptr(left) as usize) <= (Arc::as_ptr(right) as usize) {
        (left, right)
    } else {
        (right, left)
    }
}

fn order_pids<'a>(left: &'a Arc<Pid>, right: &'a Arc<Pid>) -> (&'a Arc<Pid>, &'a Arc<Pid>) {
    if (Arc::as_ptr(left) as usize) <= (Arc::as_ptr(right) as usize) {
        (left, right)
    } else {
        (right, left)
    }
}

fn find_task_in_pid_tasks(
    tasks: &[Weak<ProcessControlBlock>],
    task: &Arc<ProcessControlBlock>,
) -> Option<usize> {
    tasks
        .iter()
        .position(|candidate| Weak::ptr_eq(candidate, &task.self_ref))
}

/// 连接任务和PID的桥梁结构体
#[derive(Debug)]
pub struct PidLink {
    /// 指向对应的Pid结构体
    pub pid: RwLock<Option<Arc<Pid>>>,
}

impl PidLink {
    /// 创建新的PidLink
    pub fn new() -> Self {
        Self {
            pid: RwLock::new(None),
        }
    }

    /// 链接到指定的PID
    pub(super) fn link_pid(&self, pid: Arc<Pid>) {
        self.pid.write().replace(pid);
    }

    /// 取消PID链接
    pub(super) fn unlink_pid(&self) -> Option<Arc<Pid>> {
        self.pid.write().take()
    }

    /// 获取链接的PID
    pub fn get_pid(&self) -> Option<Arc<Pid>> {
        self.pid.read().clone()
    }

    /// 检查是否已链接PID
    #[allow(dead_code)]
    pub fn is_linked(&self) -> bool {
        self.pid.read().is_some()
    }
}

impl Default for PidLink {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for PidLink {
    fn clone(&self) -> Self {
        Self {
            pid: RwLock::new(self.get_pid()),
        }
    }
}

/// 分配一个新的PID
///
/// 参考：https://code.dragonos.org.cn/xref/linux-6.6.21/kernel/pid.c?fi=alloc_pid#162
pub(super) fn alloc_pid(ns: &Arc<PidNamespace>) -> Result<Arc<Pid>, SystemError> {
    alloc_pid_with_tids(ns, &[])
}

/// Reserve all namespace IDs before publishing any mapping. The requesting
/// caller's credentials, never a CLONE_NEWUSER child's credentials, authorize
/// each requested level. Permission results are computed outside the map lock.
pub(super) fn alloc_pid_with_tids(
    ns: &Arc<PidNamespace>,
    requested: &[i32],
) -> Result<Arc<Pid>, SystemError> {
    if requested.len() > ns.level() as usize + 1 {
        return Err(SystemError::EINVAL);
    }
    let mut chain = Vec::with_capacity(ns.level() as usize + 1);
    let mut cursor = Some(ns.clone());
    while let Some(current) = cursor {
        cursor = current.parent();
        chain.push(current);
    }
    if chain.len() != ns.level() as usize + 1 {
        return Err(SystemError::ENOMEM);
    }
    let caller = (!requested.is_empty()).then(|| ProcessManager::current_pcb().cred());
    let allowed: Vec<bool> = chain
        .iter()
        .take(requested.len())
        .map(|namespace| {
            let cred = caller
                .as_ref()
                .expect("requested IDs have caller credentials");
            super::cred::cap_capable(
                cred,
                namespace.user_ns(),
                super::cred::CAPFlags::CAP_SYS_ADMIN,
            ) || super::cred::cap_capable(
                cred,
                namespace.user_ns(),
                super::cred::CAPFlags::CAP_CHECKPOINT_RESTORE,
            )
        })
        .collect();
    let pid = Pid::new(ns.level());
    let limit = pid_max();
    let mut reserved = Vec::with_capacity(chain.len());
    let transaction = pid_map_lock();
    let result = (|| {
        for (index, namespace) in chain.iter().enumerate() {
            let wanted = requested.get(index).copied();
            if let Some(tid) = wanted {
                if tid < 1
                    || tid as usize >= limit
                    || tid != 1 && namespace.child_reaper().is_none()
                {
                    return Err(SystemError::EINVAL);
                }
                if !allowed[index] {
                    return Err(SystemError::EPERM);
                }
            }
            let nr = namespace.reserve_pid_in_ns(wanted.map(|tid| RawPid(tid as usize)), limit)?;
            reserved.push(UPid::new(nr, namespace.clone()));
        }
        // Identity is complete before a find/collect can acquire any map owner.
        {
            let mut numbers = pid.numbers.lock();
            for upid in &reserved {
                numbers[upid.ns.level() as usize] = Some(upid.clone());
            }
        }
        for upid in &reserved {
            upid.ns.publish_reserved_pid(upid.nr, pid.clone());
            pid.registered[upid.ns.level() as usize].store(true, Ordering::Release);
        }
        Ok(())
    })();
    if let Err(error) = result {
        for upid in reserved.iter().rev() {
            upid.ns.rollback_reserved_pid(upid.nr);
        }
        // Linux restores the initial cursor only for a failed first birth
        // inside alloc_pid, not for a later copy_process/free_pid failure.
        ns.reset_fresh_pid_cursor();
        drop(transaction);
        drop(reserved);
        return Err(error);
    }
    drop(transaction);
    Ok(pid)
}

/// 释放pid
///
/// 参考 https://code.dragonos.org.cn/xref/linux-6.6.21/kernel/pid.c#129
pub(super) fn free_pid(pid: Arc<Pid>) {
    // 释放PID
    pid.set_dead();
    let mut retired = Vec::with_capacity(pid.level as usize + 1);
    let mut reapers = Vec::with_capacity(pid.level as usize + 1);
    let mut dead_namespaces = Vec::with_capacity(pid.level as usize + 1);
    let transaction = pid_map_lock();
    // let raw_pid = pid.pid_vnr().data();
    let mut level = 0;
    while level <= pid.level {
        if !pid.registered[level as usize].swap(false, Ordering::AcqRel) {
            level += 1;
            continue;
        }
        let upid = pid.numbers.lock()[level as usize]
            .clone()
            .expect("registered PID namespace entry must have an identity");
        // log::debug!(
        //     "Freeing pid: raw:{}, upid.nr:{}, level: {}",
        //     raw_pid,
        //     upid.nr.data(),
        //     level
        // );
        let mut ns_guard = upid.ns.inner();
        let pid_allocated_after_free = ns_guard.do_pid_allocated() - 1;
        if pid_allocated_after_free == 1 || pid_allocated_after_free == 2 {
            if let Some(child_reaper) = ns_guard.child_reaper().as_ref().and_then(|x| x.upgrade()) {
                reapers.push(child_reaper);
            }
        }
        retired.push(ns_guard.do_release_pid_in_ns(upid.nr));
        let dead = ns_guard.dead();
        drop(ns_guard);
        if dead {
            // log::debug!("Releasing pid namespace with level {}", level);
            dead_namespaces.push(upid.ns);
        }
        level += 1;
    }
    drop(transaction);
    for reaper in reapers {
        ProcessManager::wakeup(&reaper).ok();
    }
    for namespace in dead_namespaces {
        namespace.delete_current_pidns_in_parent();
    }
    drop(retired);
}

impl ProcessControlBlock {
    pub(super) fn attach_pid(&self, pid_type: PidType) {
        let pid = self.task_pid_ptr(pid_type);
        if let Some(pid) = pid {
            self.pid_links[pid_type as usize].link_pid(pid.clone());
            pid.tasks[pid_type as usize]
                .lock_irqsave()
                .push(self.self_ref.clone());
        }
    }

    /// Transfer one physical non-thread PID link without changing the logical
    /// identity stored in the shared signal state.
    ///
    /// Linux uses transfer_pid() during de_thread() so PGID/SID indices keep
    /// exactly one entry for the thread group while its leader PCB changes.
    ///
    /// The caller must hold PID_MEMBERSHIP_LOCK.
    pub(super) fn transfer_pid_link_to_locked(
        self: &Arc<Self>,
        target: &Arc<ProcessControlBlock>,
        pid_type: PidType,
    ) {
        assert!(
            matches!(pid_type, PidType::PGID | PidType::SID),
            "only leader-owned PGID/SID links may be transferred"
        );
        assert!(
            !Arc::ptr_eq(self, target),
            "cannot transfer a PID link to the same task"
        );

        let (first, second) = order_pcbs(self, target);
        let mut first_link = first.pid_links[pid_type as usize].pid.write();
        let mut second_link = second.pid_links[pid_type as usize].pid.write();
        let (source_link, target_link) = if Arc::ptr_eq(first, self) {
            (&mut first_link, &mut second_link)
        } else {
            (&mut second_link, &mut first_link)
        };

        let pid = source_link
            .as_ref()
            .cloned()
            .expect("source task is missing its leader-owned PID link");
        assert!(
            target_link.is_none(),
            "target task already owns the transferred PID link"
        );

        let mut tasks = pid.tasks[pid_type as usize].lock_irqsave();
        let source_index = find_task_in_pid_tasks(&tasks, self)
            .expect("source task is missing from the struct Pid task list");
        assert!(
            find_task_in_pid_tasks(&tasks, target).is_none(),
            "target task is already present in the struct Pid task list"
        );
        tasks[source_index] = Arc::downgrade(target);
        **target_link = source_link.take();
    }

    pub fn task_pid_ptr(&self, pid_type: PidType) -> Option<Arc<Pid>> {
        if pid_type == PidType::PID {
            return self.thread_pid.read().clone();
        }

        self.process_signal().pid(pid_type)
    }

    pub fn task_pid_vnr(&self) -> RawPid {
        self.__task_pid_nr_ns(PidType::PID, None).unwrap()
    }

    /// 获取进程在指定PID命名空间中的PID号
    ///
    /// 根据指定的PID类型和命名空间，返回进程对应的PID值。
    /// 如果未指定命名空间，则使用当前进程的活跃PID命名空间。
    ///
    /// # 参数
    /// * `pid_type` - PID类型（进程ID、线程组ID、进程组ID或会话ID）
    /// * `ns` - 可选的PID命名空间引用，如果为None则使用当前命名空间
    ///
    /// # 返回值
    /// 返回指定命名空间中的PID值，如果进程没有对应的PID则返回None
    ///
    /// # 特殊情况
    /// * 如果进程的raw_pid为0（通常是空闲进程），则直接返回raw_pid
    #[allow(dead_code)]
    pub(crate) fn task_pid_nr_ns(
        &self,
        pid_type: PidType,
        ns: Option<Arc<PidNamespace>>,
    ) -> Option<RawPid> {
        self.__task_pid_nr_ns(pid_type, ns)
    }

    fn __task_pid_nr_ns(
        &self,
        pid_type: PidType,
        mut ns: Option<Arc<PidNamespace>>,
    ) -> Option<RawPid> {
        if self.raw_pid().data() == 0 {
            return Some(self.raw_pid());
        }
        if ns.is_none() {
            ns = Some(ProcessManager::current_pcb().active_pid_ns());
        }
        let mut retval = None;
        let ns = ns.unwrap();
        let pid = self.task_pid_ptr(pid_type);
        if let Some(pid) = pid {
            retval = Some(pid.pid_nr_ns(&ns));
        }

        return retval;
    }

    /// 获取当前任务的线程组ID (在当前PID命名空间中的TGID)
    pub fn task_tgid_vnr(&self) -> Option<RawPid> {
        self.__task_pid_nr_ns(PidType::TGID, None)
    }

    pub(super) fn detach_pid(&self, pid_type: PidType) {
        let _membership_guard =
            matches!(pid_type, PidType::PGID | PidType::SID).then(pid_membership_lock);
        self.__change_pid(pid_type, None);
    }

    /// Change a leader-owned PID identity while the caller holds
    /// PID_MEMBERSHIP_LOCK across validation and commit.
    pub(super) fn change_pid_locked(&self, pid_type: PidType, new_pid: Arc<Pid>) {
        assert!(
            matches!(pid_type, PidType::PGID | PidType::SID),
            "only leader-owned PGID/SID identities use the membership transaction"
        );
        self.__change_pid(pid_type, Some(new_pid));
        self.attach_pid(pid_type);
    }

    fn __change_pid(&self, pid_type: PidType, new_pid: Option<Arc<Pid>>) {
        // log::debug!(
        //     "Changing PID type={:?}, current_pid={:?}, new_pid={:?}",
        //     pid_type,
        //     self.task_pid_ptr(pid_type)
        //         .as_ref()
        //         .map_or("None".to_string(), |p| p.pid_vnr().data().to_string()),
        //     new_pid
        // );
        // log::debug!("current name: {}", self.basic().name());

        // Shared TGID/PGID/SID identities do not imply physical membership
        // (failed publication or exec identity transfer). PID is different:
        // the task owns its private allocation even before fork publishes it,
        // and Drop must roll that allocation back on publication failure.
        let pid = self.pid_links[pid_type as usize].unlink_pid().or_else(|| {
            (pid_type == PidType::PID)
                .then(|| self.thread_pid.read().clone())
                .flatten()
        });
        // log::debug!(
        //     "Unlinked PID type={:?}, pid={}",
        //     pid_type,
        //     pid.as_ref()
        //         .map_or("None".to_string(), |p| p.pid_vnr().data().to_string())
        // );

        if let Some(new_pid) = new_pid {
            self.init_task_pid(pid_type, new_pid.clone());
            // log::debug!(
            //     "Set new PID type={:?}, pid={:?}",
            //     pid_type,
            //     new_pid.pid_vnr().data()
            // );
        }

        if let Some(pid) = pid {
            pid.tasks[pid_type as usize]
                .lock_irqsave()
                .retain(|task| !Weak::ptr_eq(task, &self.self_ref));
            for x in PidType::ALL.iter().rev() {
                if pid.has_task(*x) {
                    // log::debug!(
                    //     "PID type={:?}, raw={} still has tasks, not freeing",
                    //     pid_type,
                    //     pid.pid_vnr().data()
                    // );
                    return;
                }
            }
            // log::debug!(
            //     "Freeing PID type={:?}, pid={:?}",
            //     pid_type,
            //     pid.pid_vnr().data()
            // );
            free_pid(pid);
        }
    }
}

impl ProcessManager {
    pub fn find_task_by_vpid(vnr: RawPid) -> Option<Arc<ProcessControlBlock>> {
        // 如果进程管理器未初始化，用旧的方法
        if !ProcessManager::initialized() {
            return Self::find(vnr);
        }

        // 如果当前进程是真实的PID 0，则用旧的方法
        let current_pcb = ProcessManager::current_pcb();
        if current_pcb.raw_pid().data() == 0 {
            return Self::find(vnr);
        }

        let active_pid_ns = current_pcb.active_pid_ns();
        return Self::find_task_by_pid_ns(vnr, &active_pid_ns);
    }

    pub fn find_task_by_pid_ns(
        nr: RawPid,
        ns: &Arc<PidNamespace>,
    ) -> Option<Arc<ProcessControlBlock>> {
        let pid: Arc<Pid> = ns.find_pid_in_ns(nr)?;
        return pid.pid_task(PidType::PID);
    }

    /// https://code.dragonos.org.cn/xref/linux-6.6.21/kernel/pid.c?fi=find_vpid#318
    pub fn find_vpid(nr: RawPid) -> Option<Arc<Pid>> {
        let active_pid_ns = ProcessManager::current_pcb().active_pid_ns();
        active_pid_ns.find_pid_in_ns(nr)
    }
}

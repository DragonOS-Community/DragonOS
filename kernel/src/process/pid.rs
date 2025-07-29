use core::fmt::Debug;
use core::sync::atomic::AtomicBool;

use crate::libs::rwlock::RwLock;
use crate::libs::spinlock::{SpinLock, SpinLockGuard};
use crate::process::ProcessManager;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use system_error::SystemError;

use super::namespace::pid_namespace::PidNamespace;
use super::{ProcessControlBlock, RawPid};

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

pub struct Pid {
    self_ref: Weak<Pid>,
    pub level: u32,
    dead: AtomicBool,
    /// 使用此PID的任务列表，按PID类型分组
    /// tasks[PidType::PID as usize] = 使用该PID作为进程ID的任务
    /// tasks[PidType::TGID as usize] = 使用该PID作为线程组ID的任务
    tasks: [SpinLock<Vec<Weak<ProcessControlBlock>>>; PidType::PIDTYPE_MAX],
    /// 在各个namespace中的PID值
    numbers: SpinLock<Vec<Option<UPid>>>,
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
        });

        pid
    }

    pub fn dead(&self) -> bool {
        self.dead.load(core::sync::atomic::Ordering::Relaxed)
    }

    pub fn set_dead(&self) {
        self.dead.store(true, core::sync::atomic::Ordering::Relaxed);
    }

    /// 获取指定PID所属的命名空间
    ///
    /// 返回该PID被分配时所在的PID命名空间的引用(Arc封装)
    pub fn ns_of_pid(&self) -> Arc<PidNamespace> {
        self.numbers
            .lock()
            .get(self.level as usize)
            .map(|upid| upid.as_ref().unwrap().ns.clone())
            .unwrap()
    }

    pub fn first_upid(&self) -> Option<UPid> {
        self.numbers.lock().first().cloned().unwrap()
    }

    pub fn tasks_iter(&self, pid_type: PidType) -> PidTaskIterator {
        let guard = self.tasks[pid_type as usize].lock();
        PidTaskIterator { guard, index: 0 }
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
        let tasks = self.tasks[pid_type as usize].lock();
        !tasks.is_empty()
    }

    pub fn pid_task(&self, pid_type: PidType) -> Option<Arc<ProcessControlBlock>> {
        let tasks = self.tasks[pid_type as usize].lock();
        if tasks.is_empty() {
            None
        } else {
            // 返回第一个进程
            tasks.first().and_then(|task| task.upgrade())
        }
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
            let upid = numbers[ns.level() as usize]
                .as_ref()
                .cloned()
                .unwrap_or_else(|| panic!("pid numbers should not be empty: ns.level={}, self.level={}, numbers.len={}", ns.level(), self.level, numbers.len()));
            if Arc::ptr_eq(&upid.ns, ns) {
                return upid.nr;
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
        // 清理numbers中的UPid引用
        let numbers_guard = self.numbers.lock();
        for upid in numbers_guard.iter() {
            if let Some(upid) = upid {
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

    /// 强制设置当前进程的raw_pid
    /// 注意：这个函数应该在创建进程时调用，不能在运行时随意调用
    pub(super) unsafe fn force_set_raw_pid(&self, pid: RawPid) {
        let self_mut = self as *const Self as *mut Self;
        (*self_mut).pid = pid;
    }
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
    pub(super) fn unlink_pid(&self) {
        self.pid.write().take();
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
    let pid = Pid::new(ns.level());

    // 用于记录已分配的PID，以便失败时清理
    let mut allocated_upids: Vec<(isize, UPid)> = Vec::new();

    // 获取当前namespace的引用链
    let mut current_ns = Some(ns.clone());
    let mut level = ns.level() as isize;

    // 从最深层级开始向上分配PID
    while level >= 0 {
        if let Some(ref curr_ns) = current_ns {
            // warn: 这里会造成Arc的循环引用，不过暂时没想到什么好办法
            // 因此需要在进程退出的时候需要手动清理pid。
            // 循环引用的路径： current_ns -> pid_map -> pid -> numbers -> upid -> ns(curr_ns)
            match curr_ns.alloc_pid_in_ns(pid.clone()) {
                Ok(nr) => {
                    let upid = UPid::new(nr, curr_ns.clone());
                    allocated_upids.push((level, upid.clone()));
                    pid.numbers.lock()[level as usize] = Some(upid);
                    current_ns = curr_ns.parent();
                }
                Err(e) => {
                    // 分配失败，需要清理已分配的PID
                    cleanup_allocated_pids(pid, allocated_upids);
                    return Err(e);
                }
            }
        }
        level -= 1;
    }

    // 如果分配成功，返回新创建的PID
    Ok(pid)
}

/// 清理已分配的PID
fn cleanup_allocated_pids(pid: Arc<Pid>, mut allocated_upids: Vec<(isize, UPid)>) {
    // 反转已分配的UPid列表，以便从最深层级开始清理
    allocated_upids.reverse();
    for (level, upid) in allocated_upids {
        let curr_ns = upid.ns;
        // 在当前namespace中释放UPid
        curr_ns.release_pid_in_ns(upid.nr);
        pid.numbers.lock()[level as usize] = None;
    }
}

/// 释放pid
///
/// 参考 https://code.dragonos.org.cn/xref/linux-6.6.21/kernel/pid.c#129
pub(super) fn free_pid(pid: Arc<Pid>) {
    // 释放PID
    pid.set_dead();
    // let raw_pid = pid.pid_vnr().data();
    let mut level = 0;
    while level <= pid.level {
        let upid = pid.numbers.lock()[level as usize]
            .take()
            .expect("pid numbers should not be empty");
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
                ProcessManager::wakeup(&child_reaper).ok();
            }
        }
        ns_guard.do_release_pid_in_ns(upid.nr);
        if ns_guard.dead() {
            // log::debug!("Releasing pid namespace with level {}", level);
            upid.ns.delete_current_pidns_in_parent();
        }
        level += 1;
    }
}

impl ProcessControlBlock {
    pub(super) fn attach_pid(&self, pid_type: PidType) {
        let pid = self.task_pid_ptr(pid_type);
        if let Some(pid) = pid {
            self.pid_links[pid_type as usize].link_pid(pid.clone());
            pid.tasks[pid_type as usize]
                .lock()
                .push(self.self_ref.clone());
        }
    }

    pub fn task_pid_ptr(&self, pid_type: PidType) -> Option<Arc<Pid>> {
        if pid_type == PidType::PID {
            return self.thread_pid.read().clone();
        }

        self.sig_struct().pids[pid_type as usize].clone()
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
    pub(super) fn task_pid_nr_ns(
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
        self.__change_pid(pid_type, None);
    }

    pub(super) fn change_pid(&self, pid_type: PidType, new_pid: Arc<Pid>) {
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

        let pid = self.task_pid_ptr(pid_type);
        self.pid_links[pid_type as usize].unlink_pid();
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
                .lock()
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

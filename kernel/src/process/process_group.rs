use super::{
    session::{Session, Sid},
    ProcessControlBlock, ProcessManager, RawPid,
};
use crate::{
    libs::spinlock::SpinLock,
    process::pid::{Pid, PidType},
};
use alloc::{
    collections::BTreeMap,
    sync::{Arc, Weak},
};
use hashbrown::HashMap;

/// 进程组ID
pub type Pgid = RawPid;

/// 系统中所有进程组
pub static ALL_PROCESS_GROUP: SpinLock<Option<HashMap<Pgid, Arc<ProcessGroup>>>> =
    SpinLock::new(None);

pub struct ProcessGroup {
    /// 进程组pgid
    pub pgid: Pgid,
    pub process_group_inner: SpinLock<PGInner>,
}

pub struct PGInner {
    pub processes: BTreeMap<RawPid, Arc<ProcessControlBlock>>,
    pub leader: Option<Arc<ProcessControlBlock>>,
    pub session: Weak<Session>,
}

impl PGInner {
    pub fn remove_process(&mut self, pid: &RawPid) {
        if let Some(process) = self.processes.remove(pid) {
            if let Some(leader) = &self.leader {
                if Arc::ptr_eq(leader, &process) {
                    self.leader = None;
                }
            }
        }
    }
    pub fn is_empty(&self) -> bool {
        self.processes.is_empty()
    }
}

impl ProcessGroup {
    pub fn new(pcb: Arc<ProcessControlBlock>) -> Arc<Self> {
        let pid = pcb.raw_pid();
        let mut processes = BTreeMap::new();
        processes.insert(pid, pcb.clone());
        let inner = PGInner {
            processes,
            leader: Some(pcb),
            session: Weak::new(),
        };
        // log::debug!("New ProcessGroup {:?}", pid);

        Arc::new(Self {
            pgid: pid,
            process_group_inner: SpinLock::new(inner),
        })
    }

    pub fn contains(&self, pid: RawPid) -> bool {
        self.process_group_inner.lock().processes.contains_key(&pid)
    }

    pub fn pgid(&self) -> Pgid {
        self.pgid
    }

    pub fn leader(&self) -> Option<Arc<ProcessControlBlock>> {
        self.process_group_inner.lock().leader.clone()
    }

    pub fn session(&self) -> Option<Arc<Session>> {
        // log::debug!("Before lock");
        let guard = self.process_group_inner.lock();
        // log::debug!("Locking");
        let session = guard.session.upgrade();
        drop(guard);
        // log::debug!("After lock");
        return session;
    }

    pub fn broadcast(&self) {
        unimplemented!("broadcast not supported yet");
    }

    pub fn sid(&self) -> Sid {
        if let Some(session) = self.session() {
            return session.sid();
        }
        Sid::from(0)
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        let mut inner = self.process_group_inner.lock();

        if let Some(leader) = inner.leader.take() {
            // 组长进程仍然在进程列表中，不应该直接销毁
            if inner.processes.contains_key(&leader.raw_pid()) {
                inner.leader = Some(leader);
            }
        }

        inner.processes.clear();

        if let Some(session) = inner.session.upgrade() {
            let mut session_inner = session.session_inner.lock();
            session_inner.process_groups.remove(&self.pgid);

            if session_inner.should_destory() {
                ProcessManager::remove_session(session.sid());
            }
        }
        // log::debug!("Dropping pg {:?}", self.pgid.clone());
    }
}

impl ProcessManager {
    /// 根据pgid获取进程组
    ///
    /// ## 参数
    ///
    /// - `pgid` : 进程组的pgid
    ///
    /// ## 返回值
    ///
    /// 如果找到了对应的进程组，那么返回该进程组，否则返回None
    #[deprecated]
    pub fn find_process_group(pgid: Pgid) -> Option<Arc<ProcessGroup>> {
        return ALL_PROCESS_GROUP
            .lock_irqsave()
            .as_ref()?
            .get(&pgid)
            .cloned();
    }

    /// 向系统中添加一个进程组
    ///
    /// ## 参数
    ///
    /// - `pg` : Arc<ProcessGroup>
    ///
    /// ## 返回值
    ///
    /// 无
    pub fn add_process_group(pg: Arc<ProcessGroup>) {
        ALL_PROCESS_GROUP
            .lock_irqsave()
            .as_mut()
            .unwrap()
            .insert(pg.pgid(), pg.clone());
        // log::debug!("New ProcessGroup added, pgid: {:?}", pg.pgid());
    }

    /// 删除一个进程组
    pub fn remove_process_group(pgid: Pgid) {
        // log::debug!("Removing pg {:?}", pgid.clone());
        let mut all_groups = ALL_PROCESS_GROUP.lock_irqsave();
        if let Some(pg) = all_groups.as_mut().unwrap().remove(&pgid) {
            // log::debug!("count: {:?}", Arc::strong_count(&pg));
            if Arc::strong_count(&pg) <= 2 {
                // 这里 Arc 计数小于等于 2，意味着它只有在 all_groups 里有一个引用，移除后会自动释放
                drop(pg);
            }
        }
    }

    // 参考 https://code.dragonos.org.cn/xref/linux-6.6.21/kernel/exit.c#345
    pub fn is_current_pgrp_orphaned() -> bool {
        let current_pcb = ProcessManager::current_pcb();
        let pgrp = current_pcb.task_pgrp().unwrap();
        Self::will_become_orphaned_pgrp(&pgrp, None)
    }

    /// 检查一个进程组是否为孤儿进程组
    ///
    /// https://code.dragonos.org.cn/xref/linux-6.6.21/kernel/exit.c#326
    #[inline(never)]
    fn will_become_orphaned_pgrp(
        pgrp: &Arc<Pid>,
        ignored_pcb: Option<&Arc<ProcessControlBlock>>,
    ) -> bool {
        for pcb in pgrp.tasks_iter(PidType::PGID) {
            let real_parent = pcb.real_parent_pcb().unwrap();
            if ignored_pcb.is_some() && Arc::ptr_eq(&pcb, ignored_pcb.unwrap()) {
                continue;
            }
            if (pcb.is_exited() && pcb.threads_read_irqsave().thread_group_empty())
                || real_parent.is_global_init()
            {
                continue;
            }

            if real_parent.task_pgrp() != Some(pgrp.clone())
                && real_parent.task_session() != pcb.task_session()
            {
                return false;
            }
        }

        return true;
    }
}

impl ProcessControlBlock {
    #[inline(always)]
    #[deprecated]
    pub fn pgid_old(&self) -> Pgid {
        if let Some(process_group) = self.process_group.lock().upgrade() {
            process_group.pgid()
        } else {
            Pgid::from(0)
        }
    }

    #[inline(always)]
    #[deprecated]
    pub fn process_group_old(&self) -> Option<Arc<ProcessGroup>> {
        self.process_group.lock().upgrade()
    }

    #[deprecated]
    pub fn set_process_group_old(&self, pg: &Arc<ProcessGroup>) {
        if let Some(pcb) = self.self_ref.upgrade() {
            *pcb.process_group.lock() = Arc::downgrade(pg);
            // log::debug!("pid: {:?} set pgid: {:?}", self.pid(), pg.pgid());
        }
    }

    /// ### 清除自身的进程组以及会话引用(如果有的话)，这个方法只能在进程退出时调用
    pub fn clear_pg_and_session_reference(&self) {
        if let Some(pg) = self.process_group_old() {
            let mut pg_inner = pg.process_group_inner.lock();
            pg_inner.remove_process(&self.raw_pid());

            if pg_inner.is_empty() {
                // 如果进程组没有任何进程了,就删除该进程组
                ProcessManager::remove_process_group(pg.pgid());
                // log::debug!("clear_pg_reference: {:?}", pg.pgid());

                if let Some(session) = pg_inner.session.upgrade() {
                    let mut session_inner = session.session_inner.lock();
                    session_inner.remove_process_group(&pg.pgid());
                    if session_inner.is_empty() {
                        // 如果会话没有任何进程组了,就删除该会话
                        ProcessManager::remove_session(session.sid());
                        // log::debug!("clear_pg_reference: {:?}", session.sid());
                    }
                }
            }
        }

        if let Some(session) = self.session_old() {
            let mut session_inner = session.session_inner.lock();
            if let Some(leader) = &session_inner.leader {
                if Arc::ptr_eq(leader, &self.self_ref.upgrade().unwrap()) {
                    session_inner.leader = None;
                }
            }
        }
    }

    pub fn task_pgrp(&self) -> Option<Arc<Pid>> {
        self.sig_struct().pids[PidType::PGID as usize].clone()
    }

    pub fn task_session(&self) -> Option<Arc<Pid>> {
        self.sig_struct().pids[PidType::SID as usize].clone()
    }

    /// 参考 https://code.dragonos.org.cn/xref/linux-6.6.21/kernel/signal.c?fi=task_join_group_stop#393
    pub(super) fn task_join_group_stop(&self) {
        // todo: 实现  https://code.dragonos.org.cn/xref/linux-6.6.21/kernel/signal.c?fi=task_join_group_stop#393
    }
}

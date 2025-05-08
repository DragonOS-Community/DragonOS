use super::{
    session::{Session, Sid},
    Pid, ProcessControlBlock, ProcessManager,
};
use crate::libs::spinlock::SpinLock;
use alloc::{
    collections::BTreeMap,
    sync::{Arc, Weak},
};
use hashbrown::HashMap;
use system_error::SystemError;

/// 进程组ID
pub type Pgid = Pid;

/// 系统中所有进程组
pub static ALL_PROCESS_GROUP: SpinLock<Option<HashMap<Pgid, Arc<ProcessGroup>>>> =
    SpinLock::new(None);

#[derive(Debug)]
pub struct ProcessGroup {
    /// 进程组pgid
    pub pgid: Pgid,
    pub process_group_inner: SpinLock<PGInner>,
}

#[derive(Debug)]
pub struct PGInner {
    pub processes: BTreeMap<Pid, Arc<ProcessControlBlock>>,
    pub leader: Option<Arc<ProcessControlBlock>>,
    pub session: Weak<Session>,
}

impl PGInner {
    pub fn remove_process(&mut self, pid: &Pid) {
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
        let pid = pcb.pid();
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

    pub fn contains(&self, pid: Pid) -> bool {
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
            if inner.processes.contains_key(&leader.pid()) {
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
        let sid = current_pcb.sid();
        let process_group = current_pcb.process_group();
        if let Some(pg) = process_group {
            for process in pg.process_group_inner.lock().processes.values() {
                if let Some(real_parent) = process.real_parent_pcb.read().clone().upgrade() {
                    //todo 添加判断： 1.是否被忽略 2.是否已经退出（线程组是否为空）
                    if real_parent.pid == Pid(1) || process.is_exited() {
                        log::debug!("is_current_pgrp_orphaned: real_parent is init or exited");
                        continue;
                    }
                    let real_parent_pg = real_parent.process_group().unwrap();
                    if real_parent_pg.pgid() != pg.pgid() && real_parent_pg.sid() == sid {
                        return false;
                    }
                }
            }
        }
        true
    }
}

impl ProcessControlBlock {
    #[inline(always)]
    pub fn pgid(&self) -> Pgid {
        if let Some(process_group) = self.process_group.lock().upgrade() {
            process_group.pgid()
        } else {
            Pgid::from(0)
        }
    }

    #[inline(always)]
    pub fn process_group(&self) -> Option<Arc<ProcessGroup>> {
        self.process_group.lock().upgrade()
    }

    pub fn set_process_group(&self, pg: &Arc<ProcessGroup>) {
        if let Some(pcb) = self.self_ref.upgrade() {
            *pcb.process_group.lock() = Arc::downgrade(pg);
            // log::debug!("pid: {:?} set pgid: {:?}", self.pid(), pg.pgid());
        }
    }

    pub fn is_process_group_leader(&self) -> bool {
        if let Some(pcb) = self.self_ref.upgrade() {
            let pg = self.process_group().unwrap();
            if let Some(leader) = pg.leader() {
                return Arc::ptr_eq(&pcb, &leader);
            }
        }

        return false;
    }

    /// 将进程加入到指定pgid的进程组中（无论该进程组是否已经存在）
    ///
    /// 如果进程组已经存在，则将进程加入到该进程组中
    /// 如果进程组不存在，则创建一个新的进程组，并将进程加入到该进程组中
    ///
    /// ## 参数
    /// `pgid` : 目标进程组的pgid
    ///
    /// ## 返回值
    /// 无
    pub fn join_other_group(&self, pgid: Pgid) -> Result<(), SystemError> {
        // if let Some(pcb) = self.self_ref.upgrade() {
        if self.pgid() == pgid {
            return Ok(());
        }
        if self.is_session_leader() {
            // 会话领导者不能加入其他进程组
            return Err(SystemError::EPERM);
        }
        if let Some(pg) = ProcessManager::find_process_group(pgid) {
            let session = self.session().unwrap();
            if !session.contains_process_group(&pg) {
                // 进程组和进程应该属于同一个会话
                return Err(SystemError::EPERM);
            }
            self.join_specified_group(&pg)?;
        } else {
            if pgid != self.pid() {
                // 进程组不存在，只能加入自己的进程组
                return Err(SystemError::EPERM);
            }
            self.join_new_group()?;
        }
        // }

        Ok(())
    }

    /// 将进程加入到新创建的进程组中
    fn join_new_group(&self) -> Result<(), SystemError> {
        let session = self.session().unwrap();
        let mut self_pg_mut = self.process_group.lock();

        if let Some(old_pg) = self_pg_mut.upgrade() {
            let mut old_pg_inner = old_pg.process_group_inner.lock();
            let mut session_inner = session.session_inner.lock();
            old_pg_inner.remove_process(&self.pid);
            *self_pg_mut = Weak::new();

            if old_pg_inner.is_empty() {
                ProcessManager::remove_process_group(old_pg.pgid());
                assert!(session_inner.process_groups.contains_key(&old_pg.pgid()));
                session_inner.process_groups.remove(&old_pg.pgid());
            }
        }

        let pcb = self.self_ref.upgrade().unwrap();
        let new_pg = ProcessGroup::new(pcb);
        let mut new_pg_inner = new_pg.process_group_inner.lock();
        let mut session_inner = session.session_inner.lock();

        *self_pg_mut = Arc::downgrade(&new_pg);
        ProcessManager::add_process_group(new_pg.clone());

        new_pg_inner.session = Arc::downgrade(&session);
        session_inner
            .process_groups
            .insert(new_pg.pgid, new_pg.clone());

        Ok(())
    }

    /// 将进程加入到指定的进程组中
    fn join_specified_group(&self, group: &Arc<ProcessGroup>) -> Result<(), SystemError> {
        let mut self_group = self.process_group.lock();

        let mut group_inner = if let Some(old_pg) = self_group.upgrade() {
            let (mut old_pg_inner, group_inner) = match old_pg.pgid().cmp(&group.pgid()) {
                core::cmp::Ordering::Equal => return Ok(()),
                core::cmp::Ordering::Less => (
                    old_pg.process_group_inner.lock(),
                    group.process_group_inner.lock(),
                ),
                core::cmp::Ordering::Greater => {
                    let group_inner = group.process_group_inner.lock();
                    let old_pg_inner = old_pg.process_group_inner.lock();
                    (old_pg_inner, group_inner)
                }
            };
            old_pg_inner.remove_process(&self.pid);
            *self_group = Weak::new();

            if old_pg_inner.is_empty() {
                ProcessManager::remove_process_group(old_pg.pgid());
            }
            group_inner
        } else {
            group.process_group_inner.lock()
        };

        let pcb = self.self_ref.upgrade().unwrap();
        group_inner.processes.insert(self.pid, pcb);
        *self_group = Arc::downgrade(group);
        Ok(())
    }

    /// ### 清除自身的进程组以及会话引用(如果有的话)，这个方法只能在进程退出时调用
    pub fn clear_pg_and_session_reference(&self) {
        if let Some(pg) = self.process_group() {
            let mut pg_inner = pg.process_group_inner.lock();
            pg_inner.remove_process(&self.pid());

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

        if let Some(session) = self.session() {
            let mut session_inner = session.session_inner.lock();
            if let Some(leader) = &session_inner.leader {
                if Arc::ptr_eq(leader, &self.self_ref.upgrade().unwrap()) {
                    session_inner.leader = None;
                }
            }
        }
    }
}

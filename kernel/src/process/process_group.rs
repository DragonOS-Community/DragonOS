use core::sync::atomic::AtomicUsize;

use super::{session::Session, Pid, ProcessControlBlock, ProcessManager};
use crate::libs::{mutex::Mutex, spinlock::SpinLock};
use alloc::{
    collections::BTreeMap,
    sync::{Arc, Weak},
};
use hashbrown::HashMap;
use system_error::SystemError;

int_like!(Pgid, AtomicPgid, usize, AtomicUsize);

/// 系统中所有进程组
pub static ALL_PROCESS_GROUP: SpinLock<Option<HashMap<Pgid, Arc<ProcessGroup>>>> =
    SpinLock::new(None);

#[derive(Debug)]
pub struct ProcessGroup {
    /// 进程组pgid
    pub pgid: Pgid,
    pub process_group_inner: Mutex<PGInner>,
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
        Arc::new(Self {
            pgid: Pgid(pid.into()),
            process_group_inner: Mutex::new(inner),
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
    }

    pub fn remove_process_group(pgid: Pgid) {
        ALL_PROCESS_GROUP
            .lock_irqsave()
            .as_mut()
            .unwrap()
            .remove(&pgid);
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

    pub fn set_process_group(self: &Arc<Self>, pg: &Arc<ProcessGroup>) {
        *self.process_group.lock() = Arc::downgrade(pg);
        // log::debug!("pid: {:?} set pgid: {:?}", self.pid(), pg.pgid());
    }

    pub fn is_process_group_leader(self: &Arc<Self>) -> bool {
        let pg = self.process_group().unwrap();
        if let Some(leader) = pg.leader() {
            return Arc::ptr_eq(self, &leader);
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
    pub fn join_other_group(self: &Arc<Self>, pgid: Pgid) -> Result<(), SystemError> {
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
            if pgid != Pgid(self.pid().into()) {
                // 进程组不存在，只能加入自己的进程组
                return Err(SystemError::EPERM);
            }
            self.join_new_group()?;
        }

        Ok(())
    }

    /// 将进程加入到新创建的进程组中
    fn join_new_group(self: &Arc<Self>) -> Result<(), SystemError> {
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

        let new_pg = ProcessGroup::new(self.clone());
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
    fn join_specified_group(
        self: &Arc<Self>,
        group: &Arc<ProcessGroup>,
    ) -> Result<(), SystemError> {
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

        group_inner.processes.insert(self.pid, self.clone());
        *self_group = Arc::downgrade(group);
        Ok(())
    }
}

use super::{
    process_group::{Pgid, ProcessGroup},
    Pid, ProcessControlBlock, ProcessManager,
};
use crate::libs::spinlock::SpinLock;
use alloc::{
    collections::BTreeMap,
    sync::{Arc, Weak},
};
use hashbrown::HashMap;
use system_error::SystemError;

/// 会话SID
pub type Sid = Pid;

/// 系统中所有会话
pub static ALL_SESSION: SpinLock<Option<HashMap<Sid, Arc<Session>>>> = SpinLock::new(None);

#[derive(Debug)]
pub struct Session {
    pub sid: Sid,
    pub session_inner: SpinLock<SessionInner>,
}

#[derive(Debug)]
pub struct SessionInner {
    pub process_groups: BTreeMap<Pgid, Arc<ProcessGroup>>,
    pub leader: Option<Arc<ProcessControlBlock>>,
}

impl SessionInner {
    pub fn is_empty(&self) -> bool {
        self.process_groups.is_empty()
    }
    pub fn remove_process_group(&mut self, pgid: &Pgid) {
        self.process_groups.remove(pgid);
    }
    pub fn remove_process(&mut self, pcb: &Arc<ProcessControlBlock>) {
        if let Some(leader) = &self.leader {
            if Arc::ptr_eq(leader, pcb) {
                self.leader = None;
            }
        }
    }
    pub fn should_destory(&self) -> bool {
        self.process_groups.is_empty()
    }
}

impl Session {
    pub fn new(group: Arc<ProcessGroup>) -> Arc<Self> {
        let sid = group.pgid;
        let mut process_groups = BTreeMap::new();
        process_groups.insert(group.pgid, group.clone());
        let inner = SessionInner {
            process_groups,
            leader: None,
        };
        // log::debug!("New Session {:?}", sid);
        Arc::new(Self {
            sid,
            session_inner: SpinLock::new(inner),
        })
    }

    pub fn sid(&self) -> Sid {
        self.sid
    }

    pub fn leader(&self) -> Option<Arc<ProcessControlBlock>> {
        self.session_inner.lock().leader.clone()
    }

    // pub fn contains_process_group(&self, pgid: Pgid) -> bool {
    //     self.session_inner.lock().process_groups.contains_key(&pgid)
    // }

    pub fn contains_process_group(&self, process_group: &Arc<ProcessGroup>) -> bool {
        self.session_inner
            .lock()
            .process_groups
            .contains_key(&process_group.pgid)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let mut session_inner = self.session_inner.lock();
        session_inner.process_groups.clear();
        session_inner.leader = None;
        // log::debug!("Dropping session: {:?}", self.sid());
    }
}

impl ProcessManager {
    /// 根据sid获取会话
    ///
    /// ## 参数
    ///
    /// - `sid` : 会话的sid
    ///
    /// ## 返回值
    ///
    /// 如果找到了对应的会话，那么返回该会话，否则返回None
    pub fn find_session(sid: Sid) -> Option<Arc<Session>> {
        return ALL_SESSION.lock_irqsave().as_ref()?.get(&sid).cloned();
    }

    /// 向系统中添加一个会话
    ///
    /// ## 参数
    ///
    /// - `session` : Arc<Session>
    ///
    /// ## 返回值
    ///
    /// 无
    pub fn add_session(session: Arc<Session>) {
        ALL_SESSION
            .lock_irqsave()
            .as_mut()
            .unwrap()
            .insert(session.sid(), session.clone());
        // log::debug!("New Session added, sid: {:?}", session.sid());
    }

    pub fn remove_session(sid: Sid) {
        // log::debug!("Removing session: {:?}", sid.clone());
        let mut all_sessions = ALL_SESSION.lock_irqsave();
        if let Some(session) = all_sessions.as_mut().unwrap().remove(&sid) {
            if Arc::strong_count(&session) <= 2 {
                // 这里 Arc 计数为 1，意味着它只有在 all_groups 里有一个引用，移除后会自动释放
                drop(session);
            }
        }
    }
}

impl ProcessControlBlock {
    pub fn session(&self) -> Option<Arc<Session>> {
        let pg = self.process_group()?;
        pg.session()
    }

    pub fn is_session_leader(&self) -> bool {
        if let Some(pcb) = self.self_ref.upgrade() {
            let session = pcb.session().unwrap();
            if let Some(leader) = session.leader() {
                return Arc::ptr_eq(&pcb, &leader);
            }
        }

        return false;
    }

    /// 将进程移动到新会话中
    /// 如果进程已经是会话领导者，则返回当前会话
    /// 如果不是，则主动创建一个新会话，并将进程移动到新会话中，返回新会话
    ///
    /// ## 返回值
    ///
    /// 新会话
    pub fn go_to_new_session(&self) -> Result<Arc<Session>, SystemError> {
        if self.is_session_leader() {
            return Ok(self.session().unwrap());
        }

        if self.is_process_group_leader() {
            return Err(SystemError::EPERM);
        }

        let session = self.session().unwrap();

        let mut self_group = self.process_group.lock();
        if ProcessManager::find_session(self.pid()).is_some() {
            return Err(SystemError::EPERM);
        }
        if ProcessManager::find_process_group(self.pid).is_some() {
            return Err(SystemError::EPERM);
        }
        if let Some(old_pg) = self_group.upgrade() {
            let mut old_pg_inner = old_pg.process_group_inner.lock();
            let mut session_inner = session.session_inner.lock();
            old_pg_inner.remove_process(&self.pid);
            *self_group = Weak::new();

            if old_pg_inner.is_empty() {
                ProcessManager::remove_process_group(old_pg.pgid());
                assert!(session_inner.process_groups.contains_key(&old_pg.pgid()));
                session_inner.process_groups.remove(&old_pg.pgid());
                if session_inner.is_empty() {
                    ProcessManager::remove_session(session.sid());
                }
            }
        }

        let pcb = self.self_ref.upgrade().unwrap();
        let new_pg = ProcessGroup::new(pcb.clone());
        *self_group = Arc::downgrade(&new_pg);
        ProcessManager::add_process_group(new_pg.clone());

        let new_session = Session::new(new_pg.clone());
        let mut new_pg_inner = new_pg.process_group_inner.lock();
        new_pg_inner.session = Arc::downgrade(&new_session);
        new_session.session_inner.lock().leader = Some(pcb.clone());
        ProcessManager::add_session(new_session.clone());

        let mut session_inner = session.session_inner.lock();
        session_inner.remove_process(&pcb);

        Ok(new_session)
    }

    pub fn sid(&self) -> Sid {
        if let Some(session) = self.session() {
            return session.sid();
        }
        return Sid::new(1);
    }
}

use super::{
    process_group::{Pgid, ProcessGroup},
    ProcessControlBlock, ProcessManager,
};
use crate::libs::{mutex::Mutex, spinlock::SpinLock};
use alloc::{
    collections::BTreeMap,
    sync::{Arc, Weak},
};
use core::sync::atomic::AtomicUsize;
use hashbrown::HashMap;
use system_error::SystemError;

int_like!(Sid, AtomicSid, usize, AtomicUsize);

/// 系统中所有会话
pub static ALL_SESSION: SpinLock<Option<HashMap<Sid, Arc<Session>>>> = SpinLock::new(None);

pub struct Session {
    pub sid: Sid,
    pub session_inner: Mutex<SessionInner>,
}

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
}

impl Session {
    pub fn new(group: Arc<ProcessGroup>) -> Arc<Self> {
        let sid = Sid(group.pgid.into());
        let mut process_groups = BTreeMap::new();
        process_groups.insert(group.pgid, group.clone());
        let inner = SessionInner {
            process_groups,
            leader: None,
        };
        Arc::new(Self {
            sid,
            session_inner: Mutex::new(inner),
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
    }

    pub fn remove_session(sid: Sid) {
        ALL_SESSION.lock_irqsave().as_mut().unwrap().remove(&sid);
    }
}

impl ProcessControlBlock {
    pub fn session(&self) -> Option<Arc<Session>> {
        let pg = self.process_group()?;
        pg.session()
    }

    pub fn is_session_leader(self: &Arc<Self>) -> bool {
        let session = self.session().unwrap();
        if let Some(leader) = session.leader() {
            return Arc::ptr_eq(self, &leader);
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
    pub fn go_to_new_session(self: &Arc<Self>) -> Result<Arc<Session>, SystemError> {
        if self.is_session_leader() {
            return Ok(self.session().unwrap());
        }

        if self.is_process_group_leader() {
            return Err(SystemError::EPERM);
        }

        let session = self.session().unwrap();

        let mut self_group = self.process_group.lock();
        if ProcessManager::find_session(Sid(self.pid().into())).is_some() {
            return Err(SystemError::EPERM);
        }
        if ProcessManager::find_process_group(Pgid::from(self.pid.into())).is_some() {
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

        let new_pg = ProcessGroup::new(self.clone());
        *self_group = Arc::downgrade(&new_pg);
        ProcessManager::add_process_group(new_pg.clone());

        let new_session = Session::new(new_pg.clone());
        let mut new_pg_inner = new_pg.process_group_inner.lock();
        new_pg_inner.session = Arc::downgrade(&new_session);
        new_session.session_inner.lock().leader = Some(self.clone());
        ProcessManager::add_session(new_session.clone());

        let mut session_inner = session.session_inner.lock();
        session_inner.remove_process(self);

        Ok(new_session)
    }
}

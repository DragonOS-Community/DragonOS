use super::{
    pid::Pid,
    process_group::{Pgid, ProcessGroup},
    ProcessControlBlock, ProcessManager, RawPid,
};
use crate::{
    driver::tty::tty_job_control::TtyJobCtrlManager, libs::spinlock::SpinLock,
    process::pid::PidType,
};
use alloc::{collections::BTreeMap, sync::Arc};
use defer::defer;
use hashbrown::HashMap;
use system_error::SystemError;

/// 会话SID
pub type Sid = RawPid;

/// 系统中所有会话
pub static ALL_SESSION: SpinLock<Option<HashMap<Sid, Arc<Session>>>> = SpinLock::new(None);

pub struct Session {
    pub sid: Sid,
    pub session_inner: SpinLock<SessionInner>,
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
    #[deprecated]
    pub fn session_old(&self) -> Option<Arc<Session>> {
        let pg = self.process_group_old()?;
        pg.session()
    }

    #[deprecated]
    pub fn sid_old(&self) -> Sid {
        if let Some(session) = self.session_old() {
            return session.sid();
        }
        return Sid::new(1);
    }
}

/// 参考 https://code.dragonos.org.cn/xref/linux-6.6.21/kernel/sys.c#1225
pub(super) fn ksys_setsid() -> Result<RawPid, SystemError> {
    let pcb = ProcessManager::current_pcb();
    let group_leader = pcb
        .threads_read_irqsave()
        .group_leader()
        .ok_or(SystemError::ESRCH)?;
    let sid = group_leader.pid();
    let session = sid.pid_vnr();
    log::debug!(
        "ksys_setsid: group_leader: {}",
        group_leader.raw_pid().data()
    );
    let siginfo_lock = group_leader.sig_info_upgradable();
    // Fail if pcb already a session leader
    if siginfo_lock.is_session_leader {
        return Err(SystemError::EPERM);
    }

    // Fail if a process group id already exists that equals the
    // proposed session id.
    if sid.pid_task(PidType::PGID).is_some() {
        return Err(SystemError::EPERM);
    }

    let mut siginfo_guard = siginfo_lock.upgrade();
    siginfo_guard.is_session_leader = true;
    set_special_pids(&group_leader, &sid);

    TtyJobCtrlManager::__proc_clear_tty(&mut siginfo_guard);
    return Ok(session);
}

fn set_special_pids(current_session_group_leader: &Arc<ProcessControlBlock>, sid: &Arc<Pid>) {
    let session = current_session_group_leader.task_session();
    let change_sid = match session {
        Some(s) => !Arc::ptr_eq(&s, sid),
        None => true,
    };

    let pgrp = current_session_group_leader.task_pgrp();
    let change_pgrp = match pgrp {
        Some(pg) => !Arc::ptr_eq(&pg, sid),
        None => true,
    };
    log::debug!(
        "leader: {}, change sid: {}, pgrp: {}, sid_raw: {}",
        current_session_group_leader.raw_pid().data(),
        change_sid,
        change_pgrp,
        sid.pid_vnr().data()
    );
    if change_sid {
        current_session_group_leader.change_pid(PidType::SID, sid.clone());
    }
    if change_pgrp {
        current_session_group_leader.change_pid(PidType::PGID, sid.clone());
    }

    log::debug!(
        "after change, pgrp: {}",
        current_session_group_leader
            .task_pgrp()
            .unwrap()
            .pid_vnr()
            .data()
    );
}

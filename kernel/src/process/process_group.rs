use super::{session::Session, Pgid, Pid, ProcessControlBlock};
use crate::libs::mutex::Mutex;
use alloc::{
    collections::BTreeMap,
    sync::{Arc, Weak},
};

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
        self.process_group_inner.lock().session.upgrade()
    }

    pub fn broadcast(&self) {
        unimplemented!("broadcast not supported yet");
    }
}

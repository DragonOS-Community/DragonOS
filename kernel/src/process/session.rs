use super::{process_group::ProcessGroup, Pgid, ProcessControlBlock, Sid};
use crate::libs::mutex::Mutex;
use alloc::{collections::BTreeMap, sync::Arc};

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
